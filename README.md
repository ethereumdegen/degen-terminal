# degen-terminal

A terminal multiplexer built in Rust. Tiling window management, tabbed workspaces, and 20 color themes — all running inside your existing terminal.

## Features

- **Tiling Panes** — Split horizontally and vertically with BSP tiling via [ratatui-hypertile](https://github.com/ratatui/ratatui-hypertile)
- **Real Shell Sessions** — Each pane runs a full PTY-backed shell with xterm-256color emulation and 1000-line scrollback
- **Tabbed Workspaces** — Multiple tabs, each with independent tiling layouts
- **20 Built-in Themes** — Midnight Blue, Dracula, Monokai Pro, Solarized, Gruvbox, Nord, Catppuccin, Tokyo Night, One Dark, Cyberpunk, Synthwave '84, Matrix, Rose Pine, Everforest, Kanagawa, Ayu Dark, Palenight, and more. Theme preference persists across sessions.
- **Mouse Support** — Drag-to-select text, click-to-focus panes, scroll through output
- **Vim-style Modal Input** — Layout mode for window management, plugin input mode for typing into shells

## Requirements

- Rust 1.88+ (edition 2024)
- A terminal with 256-color or truecolor support

## Installation

```bash
git clone https://github.com/ethereumdegen/degen-terminal.git
cd degen-terminal
cargo build --release
./target/release/degen-terminal
```

## Keybindings

### Layout Mode

| Key | Action |
|-----|--------|
| `s` | Split pane horizontally |
| `v` | Split pane vertically |
| `h` `j` `k` `l` | Navigate between panes |
| `d` | Close focused pane |
| `t` | Open theme picker |
| `i` | Enter input mode (type into shell) |
| `Ctrl+c` | Quit |

### Input Mode

| Key | Action |
|-----|--------|
| `Esc` | Return to layout mode |
| All other keys | Sent directly to the shell |

## Architecture

```
src/
  main.rs    — Event loop, tiling workspace, pane rendering, input handling
  shell.rs   — PTY shell sessions with vt100 parsing and scrollback
  theme.rs   — 20 color themes with persistence
```

Built with [ratatui](https://github.com/ratatui/ratatui) for rendering and [portable-pty](https://github.com/wez/wezterm/tree/main/pty) for real terminal emulation.

## License

MIT
