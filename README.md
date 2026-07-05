# rmux

A fast, minimal tmux-style multiplexer in Rust.

`rmux` uses Termy's embeddable terminal engine through the `termy_core` Git
dependency. A background rmux server owns the session/window/pane model and the
libtermy terminal surfaces for PTY startup, input, resize, event draining, and
frame snapshots. The foreground process is a small TUI client over a Unix
socket.

The libtermy runtime is initialized from the user's normal Termy config, so
shell, TERM/COLORTERM environment, palette queries, cursor settings, scrollback,
font metrics, and other runtime theme behavior follow the same local config
Termy uses.

## Install

With npm (downloads a prebuilt binary for macOS/Linux from GitHub Releases):

```sh
npm install -g @lassejlv/rmux
rmux
```

From source:

```sh
cargo install --path . --locked
```

Publishing a new npm version: bump the version, tag, and push. The `Release`
workflow builds the platform binaries, attaches them to the GitHub release,
and publishes `npm/` to the registry (requires the `NPM_TOKEN` repo secret).

```sh
git tag v0.1.0 && git push origin v0.1.0
```

## Run

```sh
cargo run
```

Start the first pane with a command:

```sh
cargo run -- --command "top"
```

Start or attach with tmux-shaped commands:

```sh
cargo run -- new-session -s work -c "zsh"
cargo run -- new-session -d -s worker -c "top"
cargo run -- new-session -A -s work -c "zsh"
cargo run -- new-session -d -P -F "#{session_name}:#{window_id}.#{pane_id}" -s worker
cargo run -- attach -t work
cargo run -- attach -t work:logs.1
cargo run -- list-sessions
cargo run -- list-sessions -F "#{session_name}:#{window_count}"
cargo run -- source-file ./rmux.conf
cargo run -- has-session -t work
cargo run -- wait-for -t work
cargo run -- send-keys -t work "echo hello from rmux" Enter
cargo run -- send-keys -t work --literal "literal Enter"
cargo run -- send-prefix -t work
cargo run -- wait-for -t work "hello from rmux"
cargo run -- capture-pane -t work
cargo run -- capture-pane --all-panes -t work
cargo run -- display-message -t work -p "#{session_name}:#{window_index}.#{pane_index}"
cargo run -- set-option -t work default-command "zsh"
cargo run -- display-message -t work -p "#{default_command}"
cargo run -- set-option -t work prefix C-a
cargo run -- display-message -t work -p "#{prefix_key}"
cargo run -- bind-key -t work v split-window --horizontal
cargo run -- list-keys -t work
cargo run -- split-window -t work --horizontal -c "htop"
cargo run -- split-window -t work -P -F "#{pane_id}"
cargo run -- list-panes -t work
cargo run -- list-panes -t work -F "#{pane_index}:#{pane_id}"
cargo run -- list-panes -t work -F "#{pane_index}:#{pane_active}:#{pane_width}x#{pane_height}"
cargo run -- next-pane -t work
cargo run -- previous-pane -t work
cargo run -- select-pane -t work 1
cargo run -- select-layout -t work even-vertical
cargo run -- resize-pane -t work -R 5
cargo run -- swap-pane -t work -D
cargo run -- break-pane -t work:1.2 -n broken
cargo run -- break-pane -t work:1.2 -P -F "#{window_name}:#{pane_id}"
cargo run -- join-pane -s work:broken.1 -t work:1
cargo run -- join-pane -s work:broken.1 -t work:1 -P -F "#{window_index}:#{pane_id}"
cargo run -- set-window-option -t work synchronize-panes on
cargo run -- display-message -t work -p "#{synchronize_panes}"
cargo run -- send-keys -t work:1.2 "echo sent to pane 2" Enter
cargo run -- capture-pane -t work:1.2
cargo run -- new-window -t work -n logs -c "tail -f /var/log/system.log"
cargo run -- new-window -t work -P -F "#{window_id}:#{pane_id}"
cargo run -- display-message -t work:logs.1 -p "#{window_name}:#{pane_index}"
cargo run -- display-message -t 'work:#2.%3' -p "#{window_name}:#{pane_id}"
cargo run -- next-window -t work
cargo run -- previous-window -t work
cargo run -- last-window -t work
cargo run -- swap-window -t work -U
cargo run -- move-window -t work 1
cargo run -- rename-window -t work editor
cargo run -- rename-session -t work client-a
cargo run -- list-windows -t work
cargo run -- list-windows -t work -F "#{window_index}:#{window_name}"
cargo run -- list-windows -t work -F "#{window_index}:#{window_active}:#{window_name}"
cargo run -- kill-window -t work
cargo run -- kill-pane -a -t work
cargo run -- respawn-pane -t work:1.1 -c "zsh"
cargo run -- respawn-window -t work:logs -c "zsh"
cargo run -- detach-client -t work
cargo run -- kill-session -t work
cargo run -- kill-server
```

Sessions run in a single-machine background server. `Ctrl-b d` detaches the UI
while the server keeps the session alive; `attach` reconnects to an existing
server session.

## Keys

- `Ctrl-b |` splits horizontally.
- `Ctrl-d` splits horizontally without the prefix.
- `Ctrl-Shift-d` splits vertically without the prefix.
- `Cmd-d` and `Cmd-Shift-d` also work when the terminal forwards the macOS command modifier.
- `Ctrl-t` creates a new window/tab, not a pane.
- `Ctrl-1` through `Ctrl-9` selects a window/tab (requires terminal support; use `Alt-1`–`Alt-9` or `Ctrl-b 1`–`Ctrl-b 9` if your terminal doesn't forward Ctrl+digit).
- `Alt-1` through `Alt-9` selects a window/tab (works in most terminals).
- `Ctrl-w` closes the active pane, except the last pane.
- Click inside a pane to select it.
- `Ctrl-b Ctrl-b` sends a literal prefix key to the pane.
- `set-option prefix C-a` changes the prefix key for that session.
- `bind-key v split-window --horizontal` binds a custom prefix action.
- `Ctrl-b -` splits vertically.
- `Ctrl-b n` or `Ctrl-b Right` selects the next pane.
- `Ctrl-b p` or `Ctrl-b Left` selects the previous pane.
- `Ctrl-b Space` toggles between even-horizontal and even-vertical layouts.
- `Ctrl-b H/J/K/L` resizes the active pane left/down/up/right.
- `Ctrl-b {` and `Ctrl-b }` swaps the active pane with the previous or next pane.
- `Ctrl-b x` kills the active pane, except the last pane.
- `Ctrl-b c` creates a window.
- `Ctrl-b 1` through `Ctrl-b 9` selects a window.
- `Ctrl-b l` selects the previously active window.
- `Ctrl-b d` detaches the UI process.
- `Ctrl-b q` quits.

## Check

```sh
cargo fmt --check
cargo test
cargo run -- --help
./scripts/smoke-detach-attach.sh
```
