# rmux

A fast, minimal tmux-style terminal multiplexer written in Rust and backed by
[Termy](https://github.com/lassejlv/termy).

`rmux` runs sessions in a background server, so panes and programs keep running
when you detach. The npm package installs a prebuilt binary for macOS or Linux
and verifies its SHA-256 checksum before making it executable.

## Install with Bun

The package downloads and verifies a native binary during installation, so its
postinstall script must be trusted explicitly:

```sh
bun install -g --trust @cookedoss/rmux
```

If you already installed rmux and Bun reported a blocked postinstall, run:

```sh
bun pm -g trust @cookedoss/rmux
```

## Install with npm

```sh
npm install -g @cookedoss/rmux
```

Then start or attach to the default session:

```sh
rmux
```

## Quick start

```sh
# Create and attach to a named session
rmux new-session -s work

# Start a detached session
rmux new-session -d -s worker -c "top"

# Attach later
rmux attach -t worker

# List sessions
rmux list-sessions

# Send input and capture output
rmux send-keys -t worker "echo hello" Enter
rmux capture-pane -t worker
```

## Default keys

- `Ctrl-b |` splits horizontally.
- `Ctrl-b -` splits vertically.
- `Ctrl-b c` creates a window.
- `Ctrl-b n` and `Ctrl-b p` move between panes.
- `Ctrl-b 1` through `Ctrl-b 9` select windows.
- `Ctrl-b d` detaches while keeping the session alive.
- `Ctrl-b q` quits.

Direct shortcuts such as `Ctrl-d`, `Ctrl-Shift-d`, `Ctrl-t`, and `Ctrl-w` are
also supported. Run `rmux --help` for the complete command list.

## Supported platforms

- macOS Apple silicon (`arm64`)
- macOS Intel (`x64`)
- Linux `x64`
- Linux `arm64`

To build from source instead:

```sh
cargo install --git https://github.com/lassejlv/rmux --locked
```

Source, documentation, and issues:
[github.com/lassejlv/rmux](https://github.com/lassejlv/rmux)
