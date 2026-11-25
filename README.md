# rsterm

A GPU-rendered terminal emulator for Linux, built around two goals: raw throughput and screen reader accessibility.

- **Fast**: rendering goes through wgpu (Vulkan, or GL as a fallback) as a single instanced draw per frame. The pty reader and the VT parser run on their own threads, so a program writing a lot of output is never held up by parsing or drawing.
- **Accessible**: the terminal text, tabs, menus and settings are exposed to screen readers (Orca) over AT-SPI via [AccessKit]. New output is read aloud as it arrives, and actions such as copy, search and zoom are announced.

[AccessKit]: https://accesskit.dev

## Features

- Tabs, with a tab bar that can be shown always, never, or only when more than one tab is open
- Scrollback, mouse selection, copy/paste and search in the scrollback
- Settings screen and menu that work entirely from the keyboard and with a screen reader
- Built-in themes (Tomorrow Night, High Contrast Dark and Light, Solarized Dark and Light, Gruvbox Dark, Dracula, Nord), plus custom colors
- OSC 8 hyperlinks, OSC 52 clipboard writes, colors that programs can change (OSC 4/10/11/12), synchronized output
- Font fallback through fontconfig, with box-drawing and block characters drawn procedurally so they line up exactly
- Dropdown (quake-style) mode with a global hotkey and slide animation
- Shell integration for zsh and bash (loaded automatically, no dotfile changes): jump between prompts in the scrollback, and each prompt you land on is read aloud
- Desktop notifications from programs (OSC 9 and OSC 777). When you're looking at that tab they're spoken instead of shown as a popup
- Config file that is reloaded automatically when it changes
- Configurable key bindings (Ghostty's `keybind` syntax)
- Kitty keyboard protocol, so programs such as Neovim, Helix and fish can tell every key combination apart and get key release events

## Building

You need a Rust toolchain (edition 2024), fontconfig (`fc-match`) and a Vulkan or OpenGL driver.

```sh
cargo build --release
./target/release/rsterm
```

The release profile uses fat LTO, so linking takes a few minutes.

To install it with an application menu entry:

```sh
sudo install -Dm755 target/release/rsterm /usr/local/bin/rsterm
sudo install -Dm644 rsterm.desktop /usr/local/share/applications/rsterm.desktop
```

Or just for your user: put the binary somewhere on your `$PATH` (e.g. `~/.local/bin`) and copy `rsterm.desktop` to `~/.local/share/applications/`. The desktop file runs `rsterm` from `$PATH`, so it works wherever you install the binary.

## Usage

```sh
rsterm                    # open a window running $SHELL
rsterm --dropdown         # dropdown mode: starts it the first time, toggles it after that
rsterm --dropdown show    # also: hide
rsterm --bench FILE       # headless parser benchmark, prints MB/s
```

### Keyboard shortcuts

| Keys | Action |
|---|---|
| Ctrl+Shift+T / Ctrl+Shift+W | New tab / close tab |
| Ctrl+Tab, Ctrl+PageDown | Next tab |
| Ctrl+Shift+Tab, Ctrl+PageUp | Previous tab |
| Alt+1 … Alt+8, Alt+9 | Go to that tab, Alt+9 goes to the last tab |
| Ctrl+Shift+C / Ctrl+Shift+V | Copy / paste |
| Ctrl+Shift+A | Select all |
| Ctrl+Shift+F | Search |
| Ctrl+Shift+K | Clear scrollback |
| Ctrl+Shift+PageUp / Ctrl+Shift+PageDown | Jump to previous / next shell prompt |
| Shift+PageUp / Shift+PageDown, Shift+Home / Shift+End | Scroll the scrollback |
| Ctrl+Shift+= / Ctrl+Shift+- / Ctrl+Shift+0 | Zoom in / zoom out / reset zoom |
| Ctrl+, | Settings |
| Ctrl+Shift+, | Reload config |
| Ctrl+Shift+M, Menu key, Shift+F10 | Open the menu |
| Ctrl+Shift+Q | Quit |

### Dropdown mode

`rsterm --dropdown` starts one long-lived terminal that slides in from the top or bottom of the screen (`dropdown-animation-duration`, in seconds, default 0.2; 0 turns the animation off). Press the hotkey (F12 by default, set with `dropdown-hotkey`) to show or hide it. Under Wayland the hotkey grab doesn't work; bind `rsterm --dropdown` to a shortcut in your desktop settings instead. Set `dropdown-autostart = true` to start it hidden when you log in.

Closing the last shell in the dropdown starts a new one and hides the window. Use Quit to really exit.

All of these can be changed with `keybind` (see below). The menu shows the shortcuts that are currently bound.

## Configuration

The config file is `$XDG_CONFIG_HOME/rsterm/config` (usually `~/.config/rsterm/config`). It is created with the default values on first start, uses `key = value` lines (`#` starts a comment line), and is reloaded automatically when it changes. The settings screen (Ctrl+,) edits the same file.

```ini
font-family = JetBrains Mono
font-size = 12
theme = Tomorrow Night
# block | underline | bar
cursor-style = block
cursor-style-blink = true
scrollback-limit = 10000
# auto | always | never
tab-bar = auto
copy-on-select = false
# top | bottom
dropdown-position = top
# e.g. ctrl+grave, super+Return; empty to disable
dropdown-hotkey = F12
```

All keys: `font-family`, `font-size`, `theme`, `foreground`, `background`, `cursor-color`, `selection-foreground`, `selection-background`, `scrollback-limit`, `cursor-style`, `cursor-style-blink`, `window-padding`, `window-width`, `window-height`, `window-decorations`, `tab-bar`, `copy-on-select`, `mouse-hide-while-typing`, `mouse-scroll-multiplier`, `visual-bell`, `bold-is-bright`, `window-inherit-working-directory`, `clipboard-write`, `command`, `dropdown-width`, `dropdown-height`, `dropdown-position`, `dropdown-hide-on-focus-loss`, `dropdown-always-on-top`, `dropdown-hotkey`, `dropdown-autostart`, `dropdown-animation-duration`, `desktop-notifications`, `shell-integration` (`detect` or `none`), `keybind`.

### Key bindings

`keybind` can be given any number of times. Entries apply in order, on top of the default shortcuts listed above:

```ini
keybind = ctrl+t=new_tab                 # add a binding
keybind = ctrl+shift+t=unbind            # remove one; the key goes to the program
keybind = alt+left=text:\x1bb            # send bytes (\n \r \t \e \\ \xHH \u{HEX})
keybind = ctrl+shift+u=csi:2J             # also esc:..., ignore
keybind = clear                          # remove every binding, including defaults
```

Triggers are modifiers (`ctrl`, `shift`, `alt`, `super`) joined with `+` and a key. Keys are named by their position on a US keyboard: `a`–`z`, `0`–`9`, `f1`–`f12`, `equal`, `minus`, `comma`, `period`, `slash`, `backslash`, `semicolon`, `apostrophe`, `grave`, `bracket_left`, `bracket_right`, `space`, `enter`, `tab`, `escape`, `backspace`, `insert`, `delete`, `home`, `end`, `page_up`, `page_down`, `up`, `down`, `left`, `right`, `context_menu`.

Actions: `new_tab`, `close_tab`, `next_tab`, `previous_tab`, `goto_tab:N`, `last_tab`, `copy_to_clipboard`, `paste_from_clipboard`, `select_all`, `start_search`, `clear_screen`, `jump_to_prompt:-1` / `jump_to_prompt:1`, `scroll_page_up`, `scroll_page_down`, `scroll_to_top`, `scroll_to_bottom`, `increase_font_size:1`, `decrease_font_size:1`, `reset_font_size`, `open_config`, `reload_config`, `open_menu`, `quit`, `text:...`, `csi:...`, `esc:...`, `ignore`, `unbind`.

Tab and scroll bindings let the key through to the program when they can't do anything (Alt+3 with fewer than three tabs, Shift+PageUp in full-screen programs).

### Shell integration

With `shell-integration = detect` (the default), zsh and bash are started with a small hook that marks where each prompt begins (OSC 133). rsterm sets `ZDOTDIR` for zsh and `--rcfile` for bash. Your own `.zshenv`/`.zshrc` or `.bashrc` is still loaded, and nested shells behave normally. fish 4 and newer marks its prompts without any help. When `command` is set, the shell is started unchanged.

Environment variables:

- `RSTERM_FONT`, `RSTERM_FONT_SIZE`: override the configured font
- `WGPU_BACKEND`: choose the graphics backend (e.g. `vulkan`, `gl`)

## Development

```sh
cargo test --release
cargo clippy --release
```

See [CLAUDE.md](CLAUDE.md) for an overview of the architecture and the invariants the code relies on.
