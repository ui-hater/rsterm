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
- Dropdown (quake-style) mode with a global hotkey
- Config file that is reloaded automatically when it changes

## Building

You need a Rust toolchain (edition 2024), fontconfig (`fc-match`) and a Vulkan or OpenGL driver.

```sh
cargo build --release
./target/release/rsterm
```

The release profile uses fat LTO, so linking takes a few minutes.

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
| Ctrl+Shift+= / Ctrl+Shift+- / Ctrl+Shift+0 | Zoom in / zoom out / reset zoom |
| Ctrl+, | Settings |
| Ctrl+Shift+, | Reload config |
| Ctrl+Shift+M, Menu key, Shift+F10 | Open the menu |
| Ctrl+Shift+Q | Quit |

### Dropdown mode

`rsterm --dropdown` starts one long-lived terminal that slides in from the top or bottom of the screen. Press the hotkey (F12 by default, set with `dropdown-hotkey`) to show or hide it. Under Wayland the hotkey grab doesn't work; bind `rsterm --dropdown` to a shortcut in your desktop settings instead. Set `dropdown-autostart = true` to start it hidden when you log in.

Closing the last shell in the dropdown starts a new one and hides the window. Use Quit to really exit.

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

All keys: `font-family`, `font-size`, `theme`, `foreground`, `background`, `cursor-color`, `selection-foreground`, `selection-background`, `scrollback-limit`, `cursor-style`, `cursor-style-blink`, `window-padding`, `window-width`, `window-height`, `window-decorations`, `tab-bar`, `copy-on-select`, `mouse-hide-while-typing`, `mouse-scroll-multiplier`, `visual-bell`, `bold-is-bright`, `window-inherit-working-directory`, `clipboard-write`, `command`, `dropdown-width`, `dropdown-height`, `dropdown-position`, `dropdown-hide-on-focus-loss`, `dropdown-always-on-top`, `dropdown-hotkey`, `dropdown-autostart`.

Environment variables:

- `RSTERM_FONT`, `RSTERM_FONT_SIZE`: override the configured font
- `WGPU_BACKEND`: choose the graphics backend (e.g. `vulkan`, `gl`)

## Development

```sh
cargo test --release
cargo clippy --release
```

See [CLAUDE.md](CLAUDE.md) for an overview of the architecture and the invariants the code relies on.
