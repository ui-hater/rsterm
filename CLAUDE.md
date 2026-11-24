# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

rsterm: a GPU-rendered (wgpu) terminal emulator for Linux whose screen text is exposed to screen readers (Orca) over AT-SPI via AccessKit. The two primary goals are raw throughput and accessibility; changes should not regress either.

## Commands

```sh
cargo build --release                 # release uses fat LTO + codegen-units=1 (slow link, ~3 min)
cargo test --release                  # emulator unit tests live in src/term.rs (mod tests)
cargo test --release scroll_region    # run a single test by name filter
cargo clippy --release

./target/release/rsterm               # run (spawns $SHELL on a pty)
./target/release/rsterm --bench FILE  # headless parser/emulator benchmark, prints MB/s
RSTERM_DUMP=out.ppm ./target/release/rsterm   # also render every frame offscreen and write it as PPM (debug aid)
```

Config lives in `$XDG_CONFIG_HOME/rsterm/config` (Ghostty-style `key = value`, keys defined in `Config::set`/`get`/`save` in config.rs — keep those three in sync). It is hot-reloaded via a polling watcher thread, and the settings screen rewrites the whole file. `RSTERM_FONT` / `RSTERM_FONT_SIZE` override it; `WGPU_BACKEND` overrides the default Vulkan|GL backend set. **When testing, point `XDG_CONFIG_HOME` at a scratch dir** so the settings screen doesn't overwrite the user's real config.

Testing without a visible screen / keyboard: set `SHELL` to a script that prints test output, use `RSTERM_DUMP` for visuals, and python `gi.repository.Atspi` (installed) to inspect the tree, invoke `click` actions on tabs/menu items, or listen for `object:text-changed:*` / `object:text-caret-moved` events. Debug builds (`cargo build`, `./target/debug/rsterm`) accept `RSTERM_DEBUG_SCRIPT="a:OpenMenu;k:ArrowDown;k:Enter;t:text;w:300"` (a: `Action`, k: named key into the open overlay, t: text, w: wait ms) — this drives the UI where synthetic X key events can't reach the window (e.g. a locked screen). When benchmarking with raw files, use CRLF line endings — the pty converts `\n` to `\r\n`, and bare `\n` input hits the slow wrap path and gives misleading numbers.

## Architecture

Threads and data flow:

Each tab is a `Session` (session.rs): its own pty, `Arc<Shared>` (`Mutex<Term>` + `wakeup_pending`) and two threads. `State` (main.rs) owns `tabs: Vec<Session>`, `active`, and the current `Overlay`. Session ids (not indices) are used in `UserEvent::Wakeup(id)` / `Exited(id)` because indices shift when tabs close.

- **pty-reader thread** (`session.rs`): blocking `read()` on the pty master, sends chunks over a channel immediately (buffers recycled). It never parses, so the child process is never throttled by our parsing.
- **vt-parser thread**: drains all queued chunks, runs `vte::Parser::advance` into `Term` under one `Mutex<Term>` lock, writes `Term::responses` (DA/DSR replies) back to the pty, then sends `UserEvent::Wakeup` only if `wakeup_pending` wasn't already set (coalesces to ≤1 wakeup per frame).
- **main thread** (winit `ApplicationHandler<UserEvent>`): on wakeup → `request_redraw` (active tab) or mark tab activity (background tab). Redraw rebuilds the `CellInstance` array only when a hash of (term generation, `ui_gen`, atlas epoch, focus, blink phase, ...) changes. Timers (cursor blink, visual bell, sync-output timeout, a11y throttle) are all folded into one `ControlFlow::WaitUntil` in `about_to_wait` via `next_deadline`.
- Input: `input::shortcut` maps keys to `ui::Action` (dispatched by `State::run`); otherwise `input::encode` produces xterm bytes. When an overlay is open, keys go to `State::overlay_key(named, text)` instead (decoupled from winit `KeyEvent` so the debug script can drive it).

Key invariants spanning files:

- **`Term::generation`** is the single terminal change signal: bumped on every visible mutation (`dirty()` or inline in the `print` fast path). **`State::ui_gen`** is the equivalent for everything outside the terminal (tabs, overlays, announcements) — bump it via `touch_ui()`. Both the renderer and accessibility diff against these two; a change that bumps neither won't be drawn or announced.
- **`Row::occ`** (term.rs) records how far a row has been written so `Row::reset` only clears `[..occ]` — this is what makes scrolling cheap. Every code path that writes non-default cells into a row must call `row.touch(end)`; forgetting it leaves stale characters when rows are recycled from scrollback (covered by `scrolling_into_scrollback_and_row_reuse_is_clean`).
- **`Perform::print` fast path** in term.rs handles printable ASCII without wrap/insert/charset/wide-char concerns; everything else falls to `put_char`. Keep the fast path's guard conditions in sync with what `put_char` handles.
- Grid storage: `Grid::lines` is a `VecDeque<Row>` = scrollback followed by `rows` screen lines; alt screen is a separate `Grid` with zero scrollback. Selections use absolute line numbers (`Grid::dropped` + index) so they survive scrolling.
- `Cell` is 16 bytes; the `link: u16` field (OSC 8 hyperlink id into `Term::links`) sits in what would be padding — don't grow `Cell`.
- Theme colors live in `Term::colors` (apps can change them via OSC 4/10/11/12); the renderer resolves colors from the term, not from config. `Term::set_colors` also sets the reset target for OSC 104/110/111.
- Wide chars occupy a `WIDE` cell plus a `WIDE_SPACER` cell; renderer gives the spacer the glyph's atlas x + `cell_w`, a11y skips spacers.

Rendering (`render.rs`, `shader.wgsl`, `font.rs`):

- The whole window is one cell grid: tab bar row(s) on top (`bar_rows()`), then the terminal. `build_cells` fills the terminal region, then `ui::Painter` paints the tab bar and overlays (menu, settings, search bar) as cells on top. There is no separate UI renderer.
- One instanced triangle-strip draw per frame: one 16-byte `CellInstance` per cell (atlas pos, fg, bg, flags); cell position is derived from `instance_index` in the shader. Cursor shapes/underline/strike are shader flags. Uses a non-sRGB surface format deliberately (gamma-space blending).
- `font.rs`: fonts found via `fc-match` subprocess (also used per-codepoint for fallback fonts), rasterized with fontdue into a 2048² R8 atlas of cell-sized slots; slot (0,0) is the blank glyph. Box-drawing/block elements (U+2500–U+259F) are drawn procedurally. When the atlas fills it is wiped and `Glyphs::epoch` bumps — callers rebuild cells if the epoch changed mid-build.
- wgpu here is v30: surfaces are presented with `queue.present(frame)`, and the instance needs the display handle (`new_with_display_handle_from_env`) or the GL/EGL backend can't create a surface.

Accessibility (`a11y.rs`):

- Tree: Window → [TabList → Tab×n] , Terminal of the *active tab only* (node id `term_node(tab_id)`, lines `term_node + 1 + y`, so a tab switch is a focus change, not a giant text replacement), plus the open overlay (Menu/MenuItem, Dialog with CheckBox/SpinButton/ComboBox/TextInput, SearchInput) and a `Status` live region. `TreeUpdate.focus` follows the overlay selection — that's what makes Orca speak menu/settings navigation.
- Spoken feedback for actions ("Copied", "Match 3 of 12: <line>", "Font size 12") goes through `State::announce`, which changes the live region's label.
- Text fields need a `TextRun` child to get an AT-SPI Text interface (see `text_field`); a bare `set_value` is not readable by Orca.
- Terminal rows: text trimmed (but kept up to the cursor), ending with `\n` unless soft-wrapped or the last row. Rows are sent incrementally (`prev_lines`) and terminal-output updates are throttled to `A11Y_INTERVAL`; UI changes (`ui_gen`) are pushed immediately. AccessKit itself diffs the Terminal's full document text to emit AT-SPI insert/delete events, so the text representation directly determines what Orca speaks.
- `accesskit_winit::Adapter` must be created before the window is made visible; nothing is built unless `InitialTreeRequested` has activated it. Call `a11y.reset()` whenever geometry/font changes so the next update is a full tree.
