# mockterm

A GPU-accelerated terminal emulator written in Rust with **native, draggable
pane splitting** — the Ghostty look with iTerm2-style splits. Split panes
vertically and horizontally by keyboard *or* by dragging the borders between
them, just like iTerm2.

Each pane is a real terminal running a real shell (or `tmux`, see below).

```
┌───────────────────────────────────────────────┐
│ ▌ zsh        ⌘D split  ⌘⇧D split  ⌘W close ... │
├──────────────────────┬────────────────────────┤
│ $ ls                 │ $ htop                  │
│ src  Cargo.toml      │   ▓▓▓░░ cpu             │
│ $ ▏                  │                         │
│                      ├────────────────────────┤
│                      │ $ git status            │
│                      │ $ ▏                     │
└──────────────────────┴────────────────────────┘
   drag any border ↑↔ to resize the neighbours
```

## Features

- **Native splits** — a binary tiling tree of panes, no `tmux` required. Each
  pane owns its own PTY + shell.
- **Draggable dividers** — hover the border between two panes and drag to
  resize, exactly like dragging a split in iTerm2. The cursor switches to the
  resize shape and both neighbours reflow live.
- **Keyboard shortcuts** for split / close / focus movement (below).
- **Spatial focus navigation** — `Cmd+Alt+Arrow` moves to the nearest pane in
  that direction (not just the next in a list).
- **GPU-accelerated rendering** via `eframe`/`egui` (OpenGL backend).
- **Real terminal emulation** — full VT/ANSI handling, colors, scrollback,
  selection, and copy/paste, powered by Alacritty's terminal core.
- Closing a pane (`Cmd+W`, or the shell exiting) tears down its PTY and kills
  the child process; closing the last pane quits the app.

## Keybindings

| Shortcut | Action |
|---|---|
| `Cmd+D` | Split right — new pane side-by-side (vertical divider) |
| `Cmd+Shift+D` | Split down — new pane stacked (horizontal divider) |
| `Cmd+W` | Close the focused pane |
| `Cmd+Alt+←/→/↑/↓` | Move focus to the neighbouring pane |
| drag a border | Resize the two adjacent panes |
| click a pane | Focus it |

The focused pane has an accent border, and keyboard input always goes to it
(focus does *not* require the mouse to hover the pane).

## Build & run

Requires the Rust toolchain (`rustup`).

```sh
cargo run --release            # launches your $SHELL
cargo run --release -- bash    # run a specific shell/command in each pane
cargo run --release -- --help  # usage
```

The release binary lands at `target/release/mockterm`.

## tmux

mockterm gives you native GUI splits without needing tmux at all. If you prefer
to drive panes from tmux itself, run tmux as the pane command:

```sh
mockterm tmux new -A -s main
```

Then tmux's own splits/keys work inside the pane, and you can *also* split the
mockterm window natively around it.

> **Scope note:** "native tmux support" here means native, GUI pane splitting in
> the iTerm2/Ghostty style, plus first-class support for running tmux as the
> shell. Deeper *tmux control-mode* integration (`tmux -CC`, where tmux's
> windows/panes are mirrored 1:1 onto native splits, as iTerm2 does) is a
> natural next step the architecture is built to accommodate but is not yet
> wired up — see *Roadmap*.

## Architecture

```
src/
  main.rs     CLI parsing + window bootstrap (eframe)
  app.rs      MockTerm: update loop, PTY event draining, shortcuts,
              pane rendering, draggable dividers, focus border
  layout.rs   Arena-based binary split tree + pure geometry pass
              (pane rects + divider rects) + spatial neighbour search
vendor/
  egui_term/  Vendored terminal widget (MIT, Harzu/egui_term), lightly
              patched: keyboard input follows the *focused* pane instead of
              requiring pointer hover.
```

- **Layout** (`layout.rs`) is a pure data structure: nodes are `Leaf(pane)` or
  `Split { axis, ratio, a, b }` stored in an arena. Splitting turns a leaf into
  a split; closing a leaf collapses its parent into the surviving sibling.
  Geometry is computed in one pass independent of rendering, so the same rects
  feed both drawing and keyboard navigation.
- **App** (`app.rs`) keeps a `HashMap<PaneId, TerminalBackend>`. Each backend
  runs Alacritty's PTY event loop on its own threads and forwards events over an
  `mpsc` channel that the egui update loop drains every frame.
- **Rendering** places each pane's `TerminalView` at its computed rect, then
  paints draggable divider handles and the focused-pane border on top.

### Why vendored egui_term

The upstream `egui_term` widget only processes keyboard input when a pane is
both focused **and** hovered by the mouse. For a multiplexer you want keyboard
to follow the *focused* pane regardless of the pointer, so the input gate in
`vendor/egui_term/src/view.rs` is patched: keyboard follows focus, while mouse
interaction (clicks, selection, wheel) still follows the pointer.

## Roadmap

- tmux control-mode (`tmux -CC`) integration: map tmux windows/panes onto native
  splits.
- Tabs / multiple windows.
- Config file (font, theme, default shell, keybindings).
- Zoom a pane to fullscreen and back.
- Search in scrollback.

## Credits & license

mockterm is MIT-licensed. It vendors and lightly patches
[`egui_term`](https://github.com/Harzu/egui_term) (MIT) and builds on
[`alacritty_terminal`](https://github.com/alacritty/alacritty),
[`egui`/`eframe`](https://github.com/emilk/egui), and `portable-pty`.
