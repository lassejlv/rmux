use std::{
    fs,
    io::{self, ErrorKind, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::Path,
    process::{Command as ProcessCommand, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyEvent,
    },
    execute,
    terminal::{
        EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
        size as terminal_size,
    },
};
use ratatui::{Terminal as RatTerminal, backend::CrosstermBackend};

mod ipc;
mod model;
mod protocol;
mod render;

use ipc::{
    ServerClient, connect_session, list_session_names, prepare_socket_path, read_json,
    secure_socket_path, write_json,
};
use model::{ExitReason, RawView, Rmux, TerminalConfig, build_session_view};
use protocol::{
    BoundCommand, ClientRequest, ResizeDirection, RmuxCommand, ServerResponse, SwapDirection,
    TtySize, WireKey, WireMouse,
};
use render::draw_view;

pub(crate) const STATUS_ROWS: u16 = 1;
const DEFAULT_CREATION_FORMAT: &str = "#{session_name}:#{window_index}.#{pane_index}";

#[derive(Debug, Clone, PartialEq, Eq)]
struct CliTarget {
    session: String,
    window: Option<WindowSelector>,
    pane: Option<PaneSelector>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WindowSelector {
    Index(usize),
    Id(u64),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PaneSelector {
    Index(usize),
    Id(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureMode {
    ActivePane,
    AllPanes,
}

#[derive(Debug, Parser)]
#[command(about = "A tiny tmux-style multiplexer backed by libtermy")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Command to start in the first pane.
    #[arg(long = "command", short = 'c')]
    shell_command: Option<String>,

    /// Session name for the implicit local session.
    #[arg(long, short = 's', default_value = "default")]
    session: String,

    /// Poll interval for terminal frames.
    #[arg(long, default_value_t = 16, global = true)]
    tick_ms: u64,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start a new local rmux session.
    NewSession {
        /// Session name.
        #[arg(long, short = 's')]
        session: Option<String>,

        /// Command to start in the first pane.
        #[arg(long, short = 'c')]
        command: Option<String>,

        /// Start the session server without attaching a TUI client.
        #[arg(long, short = 'd')]
        detached: bool,

        /// Attach if the named session already exists.
        #[arg(long, short = 'A')]
        attach: bool,

        /// Print target metadata after creating or finding the session and exit.
        #[arg(long, short = 'P')]
        print: bool,

        /// Format string used with -P.
        #[arg(long, short = 'F', default_value = DEFAULT_CREATION_FORMAT)]
        format: String,
    },

    /// Attach to a local rmux session name.
    Attach {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// List sessions known to this process model.
    ListSessions {
        /// Format string for each session.
        #[arg(long, short = 'F')]
        format: Option<String>,
    },

    /// Run rmux commands from a file.
    SourceFile {
        /// Path to a file containing one rmux command per line.
        path: String,
    },

    /// Check whether a session is running.
    HasSession {
        /// Target session name.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Wait until a session is live or a pane contains text.
    WaitFor {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Text to wait for in the active or targeted pane. Omit to wait for the session only.
        pattern: Option<String>,

        /// Maximum milliseconds to wait.
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },

    /// Send tmux-style key tokens to the active pane.
    SendKeys {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Send arguments literally without interpreting key token names.
        #[arg(long, short = 'l')]
        literal: bool,

        /// Key tokens or literal text. Examples: "ls", Enter, C-c, Tab, Left.
        keys: Vec<String>,
    },

    /// Send the prefix key through to the active or targeted pane.
    SendPrefix {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Bind a single prefix key to a built-in command.
    BindKey {
        /// Target session.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Single character to bind after the prefix key.
        key: char,

        /// Built-in command to run, for example split-window --horizontal.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        command: Vec<String>,
    },

    /// List custom key bindings.
    ListKeys {
        /// Target session.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Print a plain-text capture of the active or targeted pane.
    CapturePane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Capture every pane in the active window.
        #[arg(long)]
        all_panes: bool,
    },

    /// Print formatted session/window/pane metadata.
    DisplayMessage {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Format string using #{session_name}, #{window_index}, #{window_name}, #{pane_index}, #{pane_id}.
        #[arg(
            long,
            short = 'p',
            default_value = "#{session_name}:#{window_index}.#{pane_index}"
        )]
        format: String,
    },

    /// Create a new window in an existing session.
    NewWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Window name.
        #[arg(long, short = 'n')]
        name: Option<String>,

        /// Command to start in the first pane.
        #[arg(long, short = 'c')]
        command: Option<String>,

        /// Print target metadata after creating the window.
        #[arg(long, short = 'P')]
        print: bool,

        /// Format string used with -P.
        #[arg(long, short = 'F', default_value = DEFAULT_CREATION_FORMAT)]
        format: String,
    },

    /// Split the active pane in an existing session.
    SplitWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Split left/right instead of top/bottom.
        #[arg(long)]
        horizontal: bool,

        /// Command to start in the new pane.
        #[arg(long, short = 'c')]
        command: Option<String>,

        /// Print target metadata after creating the pane.
        #[arg(long, short = 'P')]
        print: bool,

        /// Format string used with -P.
        #[arg(long, short = 'F', default_value = DEFAULT_CREATION_FORMAT)]
        format: String,
    },

    /// Select a window by one-based index.
    SelectWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// One-based window index.
        index: usize,
    },

    /// Select the next window.
    NextWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Select the previous window.
    PreviousWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Select the previously active window.
    LastWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Swap the active window with its previous or next neighbor.
    SwapWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Swap with the previous window.
        #[arg(short = 'U')]
        previous: bool,

        /// Swap with the next window.
        #[arg(short = 'D')]
        next: bool,
    },

    /// Move the active window to a one-based index.
    MoveWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Destination one-based window index.
        index: usize,
    },

    /// Move the active or targeted pane into a new window.
    BreakPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// New window name.
        #[arg(long, short = 'n')]
        name: Option<String>,

        /// Print target metadata after breaking the pane out.
        #[arg(long, short = 'P')]
        print: bool,

        /// Format string used with -P.
        #[arg(long, short = 'F', default_value = DEFAULT_CREATION_FORMAT)]
        format: String,
    },

    /// Move a source pane into the active or targeted window.
    JoinPane {
        /// Source pane target to move.
        #[arg(long, short = 's')]
        source: String,

        /// Destination session/window target.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Print target metadata after joining the pane.
        #[arg(long, short = 'P')]
        print: bool,

        /// Format string used with -P.
        #[arg(long, short = 'F', default_value = DEFAULT_CREATION_FORMAT)]
        format: String,
    },

    /// Select a pane by one-based index.
    SelectPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// One-based pane index in the active window.
        index: usize,
    },

    /// Select the next pane in the active window.
    NextPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Select the previous pane in the active window.
    PreviousPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Select an even pane layout for the active window.
    SelectLayout {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Layout name: even-horizontal or even-vertical.
        layout: String,
    },

    /// Resize the active pane by changing its share of the current even layout.
    ResizePane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Grow the active pane leftward.
        #[arg(short = 'L')]
        left: bool,

        /// Grow the active pane rightward.
        #[arg(short = 'R')]
        right: bool,

        /// Grow the active pane upward.
        #[arg(short = 'U')]
        up: bool,

        /// Grow the active pane downward.
        #[arg(short = 'D')]
        down: bool,

        /// Resize amount in layout weight units.
        #[arg(default_value_t = 1)]
        amount: u16,
    },

    /// Swap the active pane with its previous or next neighbor.
    SwapPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Swap with the previous pane.
        #[arg(short = 'U')]
        previous: bool,

        /// Swap with the next pane.
        #[arg(short = 'D')]
        next: bool,
    },

    /// Kill the active pane unless it is the last pane.
    KillPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Kill all panes except the active or targeted pane.
        #[arg(long, short = 'a')]
        all_other: bool,
    },

    /// Respawn the active or targeted pane in place.
    RespawnPane {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Command to start in the pane. Omit to start the default shell.
        #[arg(long, short = 'c')]
        command: Option<String>,
    },

    /// List panes in the active window.
    ListPanes {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Format string for each pane.
        #[arg(long, short = 'F')]
        format: Option<String>,
    },

    /// List windows in an existing session.
    ListWindows {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Format string for each window.
        #[arg(long, short = 'F')]
        format: Option<String>,
    },

    /// Set a window option. Supports synchronize-panes on/off.
    SetWindowOption {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Window option name.
        option: String,

        /// Option value: on/off, true/false, or 1/0.
        value: String,
    },

    /// Set a session option. Supports default-command and prefix.
    SetOption {
        /// Target session.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Session option name.
        option: String,

        /// Option value. Use empty string or "default" to clear default-command.
        value: String,
    },

    /// Respawn the active or targeted window in place.
    RespawnWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// Command to start in the window. Omit to start the default shell.
        #[arg(long, short = 'c')]
        command: Option<String>,
    },

    /// Kill the active window unless it is the last window.
    KillWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Rename the active window in an existing session.
    RenameWindow {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// New window name.
        name: String,
    },

    /// Rename the session display name.
    RenameSession {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,

        /// New session display name.
        name: String,
    },

    /// Shut down a running session.
    KillSession {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Detach clients attached to a running session without killing it.
    DetachClient {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: Option<String>,
    },

    /// Shut down every running rmux session.
    KillServer,

    /// Run a background session server.
    #[command(hide = true)]
    Server {
        /// Session name.
        #[arg(long, short = 's')]
        session: String,

        /// Command to start in the first pane.
        #[arg(long, short = 'c')]
        command: Option<String>,
    },

    /// Send text to a running session.
    #[command(hide = true)]
    SendText {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: String,

        /// Text to send.
        text: String,
    },

    /// Print a plain-text snapshot of a running session.
    #[command(hide = true)]
    Snapshot {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: String,
    },

    /// Shut down a running session server.
    #[command(hide = true)]
    ShutdownServer {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: String,
    },

    /// Open a client connection and leave it idle for a short time.
    #[command(hide = true)]
    HoldClient {
        /// Target session, or session:window.pane; window can be index, name, or #id, pane can be index or %id.
        #[arg(long, short = 't')]
        target: String,

        /// Milliseconds to hold the socket open.
        #[arg(long, default_value_t = 1000)]
        millis: u64,

        /// File to create after the server answers the first snapshot request.
        #[arg(long)]
        ready_file: Option<String>,
    },
}

#[derive(Debug, Clone)]
struct LaunchOptions {
    session: String,
    command: Option<String>,
    tick_ms: u64,
    create_if_missing: bool,
    initial_target: Option<CliTarget>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Some(Command::ListSessions { format }) => list_sessions(format.as_deref()),
        Some(Command::SourceFile { path }) => source_file(&path, &args.session),
        Some(Command::HasSession { target }) => {
            if has_session(&target.unwrap_or(args.session)) {
                Ok(())
            } else {
                std::process::exit(1);
            }
        }
        Some(Command::WaitFor {
            target,
            pattern,
            timeout_ms,
        }) => wait_for(
            resolve_target(&args.session, target)?,
            pattern.as_deref(),
            timeout_ms,
        ),
        Some(Command::NewSession {
            session,
            command,
            detached,
            attach,
            print,
            format,
        }) => {
            let options = LaunchOptions {
                session: session.unwrap_or(args.session),
                command: command.or(args.shell_command),
                tick_ms: args.tick_ms,
                create_if_missing: true,
                initial_target: None,
            };
            if print {
                ensure_session_for_print(&options, attach)?;
                print_session_target(&options.session, &format)
            } else if detached && attach && has_session(&options.session) {
                Ok(())
            } else if detached {
                ensure_server(&options)
            } else {
                run(options)
            }
        }
        Some(Command::Attach { target }) => {
            let target = resolve_target(&args.session, target)?;
            run(LaunchOptions {
                session: target.session.clone(),
                command: args.shell_command,
                tick_ms: args.tick_ms,
                create_if_missing: false,
                initial_target: Some(target),
            })
        }
        Some(Command::SendKeys {
            target,
            literal,
            keys,
        }) => send_keys(resolve_target(&args.session, target)?, &keys, literal),
        Some(Command::SendPrefix { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::SendPrefix,
        ),
        Some(Command::BindKey {
            target,
            key,
            command,
        }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::BindKey {
                key,
                command: parse_bound_command(&command)?,
            },
        ),
        Some(Command::ListKeys { target }) => list_keys(resolve_target(&args.session, target)?),
        Some(Command::CapturePane { target, all_panes }) => capture_pane(
            resolve_target(&args.session, target)?,
            if all_panes {
                CaptureMode::AllPanes
            } else {
                CaptureMode::ActivePane
            },
        ),
        Some(Command::DisplayMessage { target, format }) => {
            display_message(resolve_target(&args.session, target)?, &format)
        }
        Some(Command::NewWindow {
            target,
            name,
            command,
            print,
            format,
        }) => {
            let view = command_view(
                resolve_target(&args.session, target)?,
                RmuxCommand::NewWindow { name, command },
            )?;
            print_command_result(print, &view, &format)
        }
        Some(Command::SplitWindow {
            target,
            horizontal,
            command,
            print,
            format,
        }) => {
            let view = command_view(
                resolve_target(&args.session, target)?,
                RmuxCommand::SplitWindow {
                    axis: if horizontal {
                        protocol::SplitAxis::Horizontal
                    } else {
                        protocol::SplitAxis::Vertical
                    },
                    command,
                },
            )?;
            print_command_result(print, &view, &format)
        }
        Some(Command::SelectWindow { target, index }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::SelectWindow {
                index: index.saturating_sub(1),
            },
        ),
        Some(Command::NextWindow { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::NextWindow,
        ),
        Some(Command::PreviousWindow { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::PreviousWindow,
        ),
        Some(Command::LastWindow { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::LastWindow,
        ),
        Some(Command::SwapWindow {
            target,
            previous,
            next,
        }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::SwapWindow {
                direction: parse_swap_direction(previous, next)?,
            },
        ),
        Some(Command::MoveWindow { target, index }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::MoveWindow {
                index: index.saturating_sub(1),
            },
        ),
        Some(Command::BreakPane {
            target,
            name,
            print,
            format,
        }) => {
            let view = command_view(
                resolve_target(&args.session, target)?,
                RmuxCommand::BreakPane { name },
            )?;
            print_command_result(print, &view, &format)
        }
        Some(Command::JoinPane {
            source,
            target,
            print,
            format,
        }) => {
            let target = resolve_target(&args.session, target)?;
            let (source_window, source_pane) = resolve_source_pane_for_join(&source, &target)?;
            let view = command_view(
                target,
                RmuxCommand::JoinPane {
                    source_window,
                    source_pane,
                },
            )?;
            print_command_result(print, &view, &format)
        }
        Some(Command::SelectPane { target, index }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::SelectPane {
                index: index.saturating_sub(1),
            },
        ),
        Some(Command::NextPane { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::NextPane,
        ),
        Some(Command::PreviousPane { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::PreviousPane,
        ),
        Some(Command::SelectLayout { target, layout }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::SelectLayout {
                axis: parse_layout(&layout)?,
            },
        ),
        Some(Command::ResizePane {
            target,
            left,
            right,
            up,
            down,
            amount,
        }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::ResizePane {
                direction: parse_resize_direction(left, right, up, down)?,
                amount,
            },
        ),
        Some(Command::SwapPane {
            target,
            previous,
            next,
        }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::SwapPane {
                direction: parse_swap_direction(previous, next)?,
            },
        ),
        Some(Command::KillPane { target, all_other }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::KillPane { all_other },
        ),
        Some(Command::RespawnPane { target, command }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::RespawnPane { command },
        ),
        Some(Command::ListPanes { target, format }) => {
            list_panes(resolve_target(&args.session, target)?, format.as_deref())
        }
        Some(Command::ListWindows { target, format }) => {
            list_windows(resolve_target(&args.session, target)?, format.as_deref())
        }
        Some(Command::SetWindowOption {
            target,
            option,
            value,
        }) => run_command(
            resolve_target(&args.session, target)?,
            parse_window_option(&option, &value)?,
        ),
        Some(Command::SetOption {
            target,
            option,
            value,
        }) => run_command(
            resolve_target(&args.session, target)?,
            parse_session_option(&option, &value)?,
        ),
        Some(Command::RespawnWindow { target, command }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::RespawnWindow { command },
        ),
        Some(Command::KillWindow { target }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::KillWindow,
        ),
        Some(Command::RenameWindow { target, name }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::RenameWindow { name },
        ),
        Some(Command::RenameSession { target, name }) => run_command(
            resolve_target(&args.session, target)?,
            RmuxCommand::RenameSession { name },
        ),
        Some(Command::KillSession { target }) => {
            kill_session(&resolve_target(&args.session, target)?.session)
        }
        Some(Command::DetachClient { target }) => {
            detach_client(resolve_target(&args.session, target)?)
        }
        Some(Command::KillServer) => kill_server(),
        Some(Command::Server { session, command }) => run_server(session, command),
        Some(Command::SendText { target, text }) => send_text(&target, &text),
        Some(Command::Snapshot { target }) => {
            capture_pane(parse_target(&target, &target)?, CaptureMode::AllPanes)
        }
        Some(Command::ShutdownServer { target }) => kill_session(&target),
        Some(Command::HoldClient {
            target,
            millis,
            ready_file,
        }) => hold_client(&target, millis, ready_file.as_deref()),
        None => run(LaunchOptions {
            session: args.session,
            command: args.shell_command,
            tick_ms: args.tick_ms,
            create_if_missing: true,
            initial_target: None,
        }),
    }
}

const CLIENT_READ_TIMEOUT_SECS: u64 = 10;

fn request_with_reconnect(
    client: &mut ServerClient,
    session: &str,
    request: ClientRequest,
) -> Result<ServerResponse> {
    match client.request(request.clone()) {
        Ok(response) => Ok(response),
        Err(err) if is_socket_closed(&err) => {
            thread::sleep(Duration::from_millis(50));
            *client = connect_session(session)?;
            client.set_read_timeout(Some(Duration::from_secs(CLIENT_READ_TIMEOUT_SECS)))?;
            client.request(request)
        }
        Err(err) => Err(err),
    }
}

fn request_paste_with_fallback(
    client: &mut ServerClient,
    session: &str,
    text: String,
    paste_request_supported: &mut bool,
) -> Result<ServerResponse> {
    if *paste_request_supported {
        match request_with_reconnect(client, session, ClientRequest::Paste(text.clone())) {
            Ok(response) => return Ok(response),
            Err(err) if is_socket_closed(&err) => {
                // A stale connection gets one fresh Paste retry above. If both attempts
                // close, the server predates the Paste variant; reconnect once more and
                // keep later pastes batched through its legacy raw Write path.
                thread::sleep(Duration::from_millis(50));
                *client = connect_session(session)?;
                client.set_read_timeout(Some(Duration::from_secs(CLIENT_READ_TIMEOUT_SECS)))?;
                *paste_request_supported = false;
            }
            Err(err) => return Err(err),
        }
    }

    request_with_reconnect(client, session, ClientRequest::Write(text.into_bytes()))
}

fn run(options: LaunchOptions) -> Result<()> {
    let mut client = match connect_session(&options.session) {
        Ok(client) => client,
        Err(_) if options.create_if_missing => {
            ensure_server(&options)?;
            connect_session(&options.session)?
        }
        Err(err) => return Err(err),
    };
    if let Some(target) = &options.initial_target {
        apply_target(target)?;
        client = connect_session(&options.session)?;
    }
    client.set_read_timeout(Some(Duration::from_secs(CLIENT_READ_TIMEOUT_SECS)))?;
    let _guard = TerminalGuard::enter()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = RatTerminal::new(backend)?;
    let tick_rate = Duration::from_millis(options.tick_ms);

    let size = available_terminal_size()?;
    let view = request_with_reconnect(&mut client, &options.session, ClientRequest::Resize(size))?
        .into_view()?;
    draw_session_view(&mut terminal, &view)?;
    let mut last_size = size;
    let mut last_frame = Instant::now();
    let mut paste_request_supported = true;

    loop {
        let mut skip_snapshot = false;
        let timeout = tick_rate.min(tick_rate.saturating_sub(last_frame.elapsed()));
        if terminal_input_ready(timeout)?
            && let Some(event) = read_terminal_event()?
        {
            let request = match event {
                Event::Resize(_, _) => {
                    let latest_size = available_terminal_size()?;
                    resize_request(latest_size, &mut last_size)
                }
                event => terminal_event_request(event),
            };
            if request.is_none() && last_frame.elapsed() < tick_rate {
                skip_snapshot = true;
            }
            let request_is_paste = matches!(&request, Some(ClientRequest::Paste(_)));
            let response = match request {
                Some(ClientRequest::Paste(text)) => Some(request_paste_with_fallback(
                    &mut client,
                    &options.session,
                    text,
                    &mut paste_request_supported,
                )?),
                Some(request) => Some(request_with_reconnect(
                    &mut client,
                    &options.session,
                    request,
                )?),
                None => None,
            };
            if let Some(response) = response {
                match response {
                    ServerResponse::View(view) => {
                        draw_session_view(&mut terminal, &view)?;
                        last_frame = Instant::now();
                        skip_snapshot = true;
                    }
                    ServerResponse::Noop
                        if !request_is_paste && last_frame.elapsed() < tick_rate =>
                    {
                        skip_snapshot = true;
                    }
                    ServerResponse::Noop => {}
                    ServerResponse::Detached | ServerResponse::Shutdown => return Ok(()),
                    ServerResponse::Error(message) => return Err(anyhow!(message)),
                };
            }
        }

        if skip_snapshot {
            continue;
        }

        let response =
            request_with_reconnect(&mut client, &options.session, ClientRequest::Snapshot)?;
        match response {
            ServerResponse::View(view) => {
                draw_session_view(&mut terminal, &view)?;
                last_frame = Instant::now();
            }
            ServerResponse::Noop => {}
            ServerResponse::Detached | ServerResponse::Shutdown => break,
            ServerResponse::Error(message) => return Err(anyhow!(message)),
        };
    }

    Ok(())
}

fn terminal_event_request(event: Event) -> Option<ClientRequest> {
    match event {
        Event::Key(key) => Some(ClientRequest::Key(WireKey::from(key))),
        Event::Paste(text) if !text.is_empty() => Some(ClientRequest::Paste(text)),
        Event::Mouse(mouse) => Some(ClientRequest::Mouse(WireMouse::from(mouse))),
        _ => None,
    }
}

fn resize_request(latest_size: TtySize, last_size: &mut TtySize) -> Option<ClientRequest> {
    if latest_size == *last_size {
        return None;
    }
    *last_size = latest_size;
    Some(ClientRequest::Resize(latest_size))
}

fn ensure_server(options: &LaunchOptions) -> Result<()> {
    let mut command = ProcessCommand::new(std::env::current_exe().context("current executable")?);
    command
        .arg("server")
        .arg("--session")
        .arg(&options.session)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.process_group(0);
    if let Some(shell_command) = &options.command {
        command.arg("--command").arg(shell_command);
    }
    command.spawn().context("spawn rmux server")?;

    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if connect_session(&options.session).is_ok() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(25));
    }

    Err(anyhow!("rmux server did not start for {}", options.session))
}

fn ensure_session_for_print(options: &LaunchOptions, attach_if_exists: bool) -> Result<()> {
    if attach_if_exists && has_session(&options.session) {
        return Ok(());
    }
    ensure_server(options)
}

fn source_file(path: impl AsRef<Path>, default_session: &str) -> Result<()> {
    let path = path.as_ref();
    let contents =
        fs::read_to_string(path).with_context(|| format!("read source-file {}", path.display()))?;
    for (line_index, raw_line) in contents.lines().enumerate() {
        let Some(args) = parse_source_line(raw_line)
            .with_context(|| format!("parse {}:{}", path.display(), line_index + 1))?
        else {
            continue;
        };
        run_sourced_command(path, line_index + 1, default_session, args)?;
    }
    Ok(())
}

fn run_sourced_command(
    path: &Path,
    line_number: usize,
    default_session: &str,
    args: Vec<String>,
) -> Result<()> {
    let executable = std::env::current_exe().context("current executable")?;
    let mut command = ProcessCommand::new(executable);
    command
        .arg("--session")
        .arg(default_session)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let status = command
        .status()
        .with_context(|| format!("run sourced command {}:{}", path.display(), line_number))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "source-file command failed at {}:{} with {}",
            path.display(),
            line_number,
            status
        ))
    }
}

fn parse_source_line(line: &str) -> Result<Option<Vec<String>>> {
    let trimmed = line.trim_start();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(None);
    }

    let mut args = Vec::new();
    let mut token = String::new();
    let mut chars = line.chars().peekable();
    let mut quote = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '#') if token.is_empty() && args.is_empty() => return Ok(None),
            (None, ch) if ch.is_whitespace() => {
                if !token.is_empty() {
                    args.push(std::mem::take(&mut token));
                }
            }
            (None, '\'' | '"') => quote = Some(ch),
            (Some(active), ch) if ch == active => quote = None,
            (_, '\\') => {
                let escaped = chars
                    .next()
                    .ok_or_else(|| anyhow!("line ends with escape"))?;
                token.push(escaped);
            }
            (_, ch) => token.push(ch),
        }
    }
    if let Some(active) = quote {
        return Err(anyhow!("unterminated {active:?} quote"));
    }
    if !token.is_empty() {
        args.push(token);
    }
    if args.first().is_some_and(|arg| arg == "rmux") {
        args.remove(0);
    }
    Ok(Some(args).filter(|args| !args.is_empty()))
}

fn resolve_target(default_session: &str, target: Option<String>) -> Result<CliTarget> {
    parse_target(
        target.as_deref().unwrap_or(default_session),
        default_session,
    )
}

fn parse_target(raw: &str, default_session: &str) -> Result<CliTarget> {
    let (session_part, selector_part) = raw.split_once(':').unwrap_or((raw, ""));
    let session = if session_part.is_empty() {
        default_session.to_string()
    } else {
        session_part.to_string()
    };

    let (window, pane) = if selector_part.is_empty() {
        (None, None)
    } else {
        let (window_part, pane_part) = selector_part.split_once('.').unwrap_or((selector_part, ""));
        let window = parse_window_selector(window_part)?;
        let pane = if pane_part.is_empty() {
            None
        } else {
            parse_pane_selector(pane_part)?
        };
        (window, pane)
    };

    Ok(CliTarget {
        session,
        window,
        pane,
    })
}

fn parse_one_based_target_index(value: &str, label: &str) -> Result<Option<usize>> {
    if value.is_empty() {
        return Ok(None);
    }
    let parsed = value
        .parse::<usize>()
        .with_context(|| format!("parse target {label} index {value:?}"))?;
    if parsed == 0 {
        return Err(anyhow!("target {label} index must be one-based"));
    }
    Ok(Some(parsed - 1))
}

fn parse_window_selector(value: &str) -> Result<Option<WindowSelector>> {
    if value.is_empty() {
        return Ok(None);
    }
    if let Some(id) = value.strip_prefix('#') {
        return Ok(Some(WindowSelector::Id(parse_target_id(id, "window")?)));
    }
    match value.parse::<usize>() {
        Ok(0) => Err(anyhow!("target window index must be one-based")),
        Ok(index) => Ok(Some(WindowSelector::Index(index - 1))),
        Err(_) => Ok(Some(WindowSelector::Name(value.to_string()))),
    }
}

fn parse_pane_selector(value: &str) -> Result<Option<PaneSelector>> {
    if let Some(id) = value.strip_prefix('%') {
        return Ok(Some(PaneSelector::Id(parse_target_id(id, "pane")?)));
    }
    Ok(parse_one_based_target_index(value, "pane")?.map(PaneSelector::Index))
}

fn parse_target_id(value: &str, label: &str) -> Result<u64> {
    if value.is_empty() {
        return Err(anyhow!("target {label} id is empty"));
    }
    value
        .parse::<u64>()
        .with_context(|| format!("parse target {label} id {value:?}"))
}

fn parse_layout(value: &str) -> Result<protocol::SplitAxis> {
    match value {
        "even-horizontal" | "horizontal" => Ok(protocol::SplitAxis::Horizontal),
        "even-vertical" | "vertical" => Ok(protocol::SplitAxis::Vertical),
        _ => Err(anyhow!(
            "unsupported layout {value:?}; expected even-horizontal or even-vertical"
        )),
    }
}

fn parse_resize_direction(
    left: bool,
    right: bool,
    up: bool,
    down: bool,
) -> Result<ResizeDirection> {
    let directions = [
        (left, ResizeDirection::Left, "-L"),
        (right, ResizeDirection::Right, "-R"),
        (up, ResizeDirection::Up, "-U"),
        (down, ResizeDirection::Down, "-D"),
    ]
    .into_iter()
    .filter(|(enabled, _, _)| *enabled)
    .collect::<Vec<_>>();

    match directions.as_slice() {
        [(_, direction, _)] => Ok(*direction),
        [] => Err(anyhow!(
            "resize-pane needs one direction: -L, -R, -U, or -D"
        )),
        _ => Err(anyhow!("resize-pane accepts only one direction flag")),
    }
}

fn parse_swap_direction(previous: bool, next: bool) -> Result<SwapDirection> {
    match (previous, next) {
        (true, false) => Ok(SwapDirection::Previous),
        (false, true) => Ok(SwapDirection::Next),
        (false, false) => Err(anyhow!("swap-pane needs one direction: -U or -D")),
        (true, true) => Err(anyhow!("swap-pane accepts only one direction flag")),
    }
}

fn parse_window_option(option: &str, value: &str) -> Result<RmuxCommand> {
    match option {
        "synchronize-panes" | "synchronize_panes" => Ok(RmuxCommand::SetSynchronizePanes {
            enabled: parse_on_off(value)?,
        }),
        _ => Err(anyhow!(
            "unsupported window option {option:?}; expected synchronize-panes"
        )),
    }
}

fn parse_session_option(option: &str, value: &str) -> Result<RmuxCommand> {
    match option {
        "default-command" | "default_command" => Ok(RmuxCommand::SetDefaultCommand {
            command: parse_default_command(value),
        }),
        "prefix" | "prefix-key" | "prefix_key" => Ok(RmuxCommand::SetPrefixKey {
            key: parse_prefix_key(value)?,
        }),
        _ => Err(anyhow!(
            "unsupported session option {option:?}; expected default-command or prefix"
        )),
    }
}

fn parse_bound_command(tokens: &[String]) -> Result<BoundCommand> {
    let Some(command) = tokens.first().map(String::as_str) else {
        return Err(anyhow!("bind-key needs a command"));
    };
    match command {
        "split-window" => {
            if tokens
                .iter()
                .skip(1)
                .any(|token| token == "--horizontal" || token == "-h")
            {
                Ok(BoundCommand::SplitHorizontal)
            } else {
                Ok(BoundCommand::SplitVertical)
            }
        }
        "new-window" => Ok(BoundCommand::NewWindow),
        "next-pane" => Ok(BoundCommand::NextPane),
        "previous-pane" => Ok(BoundCommand::PreviousPane),
        "kill-pane" => Ok(BoundCommand::KillPane),
        "detach-client" | "detach" => Ok(BoundCommand::DetachClient),
        "last-window" => Ok(BoundCommand::LastWindow),
        "select-layout" if tokens.get(1).is_some_and(|layout| layout == "next") => {
            Ok(BoundCommand::ToggleLayout)
        }
        _ => Err(anyhow!(
            "unsupported bind-key command {command:?}; supported: split-window, new-window, next-pane, previous-pane, kill-pane, detach-client, last-window, select-layout next"
        )),
    }
}

fn parse_default_command(value: &str) -> Option<String> {
    match value {
        "" | "default" => None,
        _ => Some(value.to_string()),
    }
}

fn parse_prefix_key(value: &str) -> Result<u8> {
    let value = value.trim();
    let mut chars = value.chars();
    if let (Some(first), Some(second), Some(third), None) =
        (chars.next(), chars.next(), chars.next(), chars.next())
        && first.eq_ignore_ascii_case(&'C')
        && second == '-'
    {
        return control_key_byte(third.to_ascii_lowercase())
            .ok_or_else(|| anyhow!("unsupported prefix key {value:?}"));
    }
    Err(anyhow!(
        "unsupported prefix key {value:?}; expected C-a through C-z or C-["
    ))
}

fn parse_on_off(value: &str) -> Result<bool> {
    match value.to_ascii_lowercase().as_str() {
        "on" | "1" | "true" => Ok(true),
        "off" | "0" | "false" => Ok(false),
        _ => Err(anyhow!("expected on/off, true/false, or 1/0")),
    }
}

fn apply_target(target: &CliTarget) -> Result<()> {
    let view = if target.window.is_some() {
        let mut client = connect_session(&target.session)?;
        Some(client.request(ClientRequest::Snapshot)?.into_view()?)
    } else {
        None
    };
    if let Some(window) = resolve_window_selector(target, view.as_ref())? {
        let mut client = connect_session(&target.session)?;
        client
            .request(ClientRequest::Command(RmuxCommand::SelectWindow {
                index: window,
            }))?
            .into_view()
            .with_context(|| format!("select target window {}", window + 1))?;
    }
    let pane_view = if matches!(&target.pane, Some(PaneSelector::Id(_))) {
        let mut client = connect_session(&target.session)?;
        Some(client.request(ClientRequest::Snapshot)?.into_view()?)
    } else {
        None
    };
    if let Some(pane) = resolve_pane_selector(target, pane_view.as_ref())? {
        let mut client = connect_session(&target.session)?;
        client
            .request(ClientRequest::Command(RmuxCommand::SelectPane {
                index: pane,
            }))?
            .into_view()
            .with_context(|| format!("select target pane {}", pane + 1))?;
    }
    Ok(())
}

fn resolve_source_pane_for_join(source: &str, destination: &CliTarget) -> Result<(usize, usize)> {
    let source = parse_target(source, &destination.session)?;
    if source.session != destination.session {
        return Err(anyhow!(
            "join-pane source and target must be in the same session"
        ));
    }
    apply_target(&source)?;
    let mut client = connect_session(&source.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    Ok((view.active_window, view.active_pane))
}

fn resolve_window_selector(
    target: &CliTarget,
    view: Option<&protocol::SessionView>,
) -> Result<Option<usize>> {
    let Some(selector) = &target.window else {
        return Ok(None);
    };
    match selector {
        WindowSelector::Index(index) => Ok(Some(*index)),
        WindowSelector::Id(id) => {
            let view = view.ok_or_else(|| anyhow!("target snapshot missing"))?;
            view.windows
                .iter()
                .position(|window| window.id == *id)
                .map(Some)
                .ok_or_else(|| anyhow!("window #{id} does not exist"))
        }
        WindowSelector::Name(name) => {
            let view = view.ok_or_else(|| anyhow!("target snapshot missing"))?;
            view.windows
                .iter()
                .position(|window| window.name == *name)
                .map(Some)
                .ok_or_else(|| anyhow!("window {name:?} does not exist"))
        }
    }
}

fn resolve_pane_selector(
    target: &CliTarget,
    view: Option<&protocol::SessionView>,
) -> Result<Option<usize>> {
    let Some(selector) = &target.pane else {
        return Ok(None);
    };
    match selector {
        PaneSelector::Index(index) => Ok(Some(*index)),
        PaneSelector::Id(id) => {
            let view = view.ok_or_else(|| anyhow!("target snapshot missing"))?;
            view.panes
                .iter()
                .position(|pane| pane.id == *id)
                .map(Some)
                .ok_or_else(|| anyhow!("pane %{id} does not exist"))
        }
    }
}

fn send_text(session: &str, text: &str) -> Result<()> {
    let mut client = connect_session(session)?;
    let response = client.request(ClientRequest::Write(text.as_bytes().to_vec()))?;
    response.into_view().map(|_| ())
}

fn send_keys(target: CliTarget, keys: &[String], literal: bool) -> Result<()> {
    let bytes = encode_send_keys(keys, literal)?;
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    let response = client.request(ClientRequest::Write(bytes))?;
    response.into_view().map(|_| ())
}

fn encode_send_keys(keys: &[String], literal: bool) -> Result<Vec<u8>> {
    if literal {
        Ok(keys.concat().into_bytes())
    } else {
        encode_key_tokens(keys)
    }
}

fn encode_key_tokens(keys: &[String]) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for key in keys {
        bytes.extend(encode_key_token(key)?);
    }
    Ok(bytes)
}

fn encode_key_token(key: &str) -> Result<Vec<u8>> {
    let normalized = key.to_ascii_lowercase();
    let bytes = match normalized.as_str() {
        "enter" | "return" | "c-m" => b"\r".to_vec(),
        "c-j" => b"\n".to_vec(),
        "space" => b" ".to_vec(),
        "tab" | "c-i" => b"\t".to_vec(),
        "escape" | "esc" | "c-[" => b"\x1b".to_vec(),
        "backspace" | "bs" => vec![0x7f],
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "right" => b"\x1b[C".to_vec(),
        "left" => b"\x1b[D".to_vec(),
        "home" => b"\x1b[H".to_vec(),
        "end" => b"\x1b[F".to_vec(),
        "delete" | "dc" => b"\x1b[3~".to_vec(),
        _ if normalized.starts_with("c-") && normalized.len() == 3 => {
            let ch = normalized
                .chars()
                .nth(2)
                .ok_or_else(|| anyhow!("invalid control token {key}"))?;
            let byte =
                control_key_byte(ch).ok_or_else(|| anyhow!("unsupported control token {key}"))?;
            vec![byte]
        }
        _ => key.as_bytes().to_vec(),
    };
    Ok(bytes)
}

fn control_key_byte(ch: char) -> Option<u8> {
    if ch.is_ascii_lowercase() {
        Some((ch as u8) - b'a' + 1)
    } else if ch == '[' {
        Some(0x1b)
    } else {
        None
    }
}

fn run_command(target: CliTarget, command: RmuxCommand) -> Result<()> {
    command_view(target, command).map(|_| ())
}

fn detach_client(target: CliTarget) -> Result<()> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    match client.request(ClientRequest::Command(RmuxCommand::DetachClient))? {
        ServerResponse::View(_) | ServerResponse::Noop | ServerResponse::Detached => Ok(()),
        ServerResponse::Shutdown => Err(anyhow!("session shut down")),
        ServerResponse::Error(message) => Err(anyhow!(message)),
    }
}

fn command_view(target: CliTarget, command: RmuxCommand) -> Result<protocol::SessionView> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    client.request(ClientRequest::Command(command))?.into_view()
}

fn print_command_result(print: bool, view: &protocol::SessionView, format: &str) -> Result<()> {
    if print {
        let rendered = render_format(view, format)?;
        let stdout = io::stdout();
        let mut out = stdout.lock();
        write_stdout_line(&mut out, format_args!("{rendered}"))?;
    }
    Ok(())
}

fn print_session_target(session: &str, format: &str) -> Result<()> {
    let mut client = connect_session(session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    print_command_result(true, &view, format)
}

fn list_windows(target: CliTarget, format: Option<&str>) -> Result<()> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (index, window) in view.windows.iter().enumerate() {
        if let Some(format) = format {
            write_stdout_line(
                &mut out,
                format_args!("{}", render_window_format(&view, index, format)?),
            )?;
        } else {
            let marker = if index == view.active_window {
                "*"
            } else {
                " "
            };
            write_stdout_line(
                &mut out,
                format_args!("{marker}{}:{}#{}", index + 1, window.name, window.id),
            )?;
        }
    }
    Ok(())
}

fn list_panes(target: CliTarget, format: Option<&str>) -> Result<()> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for (index, pane) in view.panes.iter().enumerate() {
        if let Some(format) = format {
            write_stdout_line(
                &mut out,
                format_args!("{}", render_pane_format(&view, index, format)?),
            )?;
        } else {
            let marker = if index == view.active_pane { "*" } else { " " };
            write_stdout_line(&mut out, format_args!("{marker}{}:%{}", index + 1, pane.id))?;
        }
    }
    Ok(())
}

fn list_keys(target: CliTarget) -> Result<()> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for binding in &view.key_bindings {
        write_stdout_line(
            &mut out,
            format_args!(
                "bind-key {} {}",
                binding.key,
                binding.command.command_name()
            ),
        )?;
    }
    Ok(())
}

fn capture_pane(target: CliTarget, mode: CaptureMode) -> Result<()> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        CaptureMode::ActivePane => {
            if let Some(pane) = view.panes.get(view.active_pane) {
                write_pane_capture(&mut out, pane)?;
            }
        }
        CaptureMode::AllPanes => {
            for pane in &view.panes {
                write_pane_capture(&mut out, pane)?;
            }
        }
    }
    Ok(())
}

fn display_message(target: CliTarget, format: &str) -> Result<()> {
    apply_target(&target)?;
    let mut client = connect_session(&target.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    let rendered = render_format(&view, format)?;
    let stdout = io::stdout();
    let mut out = stdout.lock();
    write_stdout_line(&mut out, format_args!("{rendered}"))
}

fn render_format(view: &protocol::SessionView, format: &str) -> Result<String> {
    render_scoped_format(view, view.active_window, view.active_pane, format)
}

fn render_scoped_format(
    view: &protocol::SessionView,
    window_index: usize,
    pane_index: usize,
    format: &str,
) -> Result<String> {
    let active_window = view
        .windows
        .get(window_index)
        .ok_or_else(|| anyhow!("active window is missing"))?;
    let active_pane = view
        .panes
        .get(pane_index)
        .ok_or_else(|| anyhow!("active pane is missing"))?;

    let window_id = active_window.id.to_string();
    let pane_id = active_pane.id.to_string();
    let window_number = (window_index + 1).to_string();
    let pane_number = (pane_index + 1).to_string();
    let window_count = view.windows.len().to_string();
    let pane_count = view.panes.len().to_string();
    let default_command = view.default_command.as_deref().unwrap_or("");
    let prefix_key = view.prefix_key.as_str();
    let window_active = if window_index == view.active_window {
        "1"
    } else {
        "0"
    };
    let pane_active = if pane_index == view.active_pane {
        "1"
    } else {
        "0"
    };
    let pane_width = active_pane.cols.to_string();
    let pane_height = active_pane.rows.to_string();
    let pane_weight = view
        .pane_weights
        .get(pane_index)
        .copied()
        .unwrap_or(1)
        .to_string();
    let synchronize_panes = if active_window.synchronize_panes {
        "1"
    } else {
        "0"
    };
    let pane_weights = view
        .pane_weights
        .iter()
        .map(u16::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let window_layout = view.split_axis.layout_name();
    let replacements = [
        ("#{session_name}", view.session.as_str()),
        ("#{window_name}", active_window.name.as_str()),
        ("#{window_id}", window_id.as_str()),
        ("#{pane_id}", pane_id.as_str()),
        ("#{window_index}", window_number.as_str()),
        ("#{pane_index}", pane_number.as_str()),
        ("#{default_command}", default_command),
        ("#{prefix_key}", prefix_key),
        ("#{window_count}", window_count.as_str()),
        ("#{pane_count}", pane_count.as_str()),
        ("#{window_active}", window_active),
        ("#{pane_active}", pane_active),
        ("#{pane_width}", pane_width.as_str()),
        ("#{pane_height}", pane_height.as_str()),
        ("#{pane_weight}", pane_weight.as_str()),
        ("#{synchronize_panes}", synchronize_panes),
        ("#{pane_weights}", pane_weights.as_str()),
        ("#{window_layout}", window_layout),
    ];

    let mut rendered = format.to_string();
    for (needle, value) in replacements {
        rendered = rendered.replace(needle, value);
    }
    Ok(rendered)
}

fn render_window_format(
    view: &protocol::SessionView,
    window_index: usize,
    format: &str,
) -> Result<String> {
    render_scoped_format(view, window_index, view.active_pane, format)
}

fn render_pane_format(
    view: &protocol::SessionView,
    pane_index: usize,
    format: &str,
) -> Result<String> {
    render_scoped_format(view, view.active_window, pane_index, format)
}

fn write_pane_capture(out: &mut impl Write, pane: &protocol::PaneView) -> Result<()> {
    write_stdout_line(out, format_args!("== pane {} ==", pane.id))?;
    for line in &pane.lines {
        write_stdout_line(out, format_args!("{}", line.trim_end()))?;
    }
    Ok(())
}

fn kill_session(session: &str) -> Result<()> {
    let mut client = connect_session(session)?;
    match client.request(ClientRequest::Shutdown)? {
        ServerResponse::Shutdown => Ok(()),
        other => Err(anyhow!("unexpected shutdown response: {other:?}")),
    }
}

fn kill_server() -> Result<()> {
    let mut failures = Vec::new();
    for session in list_session_names()? {
        if let Err(err) = kill_session(&session) {
            failures.push(format!("{session}: {err}"));
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(anyhow!(
            "failed to kill rmux sessions: {}",
            failures.join("; ")
        ))
    }
}

fn hold_client(session: &str, millis: u64, mut ready_file: Option<&str>) -> Result<()> {
    let mut client = connect_session(session)?;
    let deadline = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < deadline {
        let response = match client.request(ClientRequest::Snapshot) {
            Ok(response) => response,
            Err(err) if is_socket_closed(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        match response {
            ServerResponse::View(_) | ServerResponse::Noop => {
                if let Some(path) = ready_file.take() {
                    fs::write(path, b"ready\n").context("write hold-client ready file")?;
                }
                thread::sleep(Duration::from_millis(25))
            }
            ServerResponse::Detached | ServerResponse::Shutdown => return Ok(()),
            ServerResponse::Error(message) => return Err(anyhow!(message)),
        }
    }
    Ok(())
}

fn is_socket_closed(err: &anyhow::Error) -> bool {
    err.downcast_ref::<io::Error>().is_some_and(|io_err| {
        matches!(
            io_err.kind(),
            ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::UnexpectedEof
        )
    })
}

const MAX_CLIENT_CONNECTIONS: usize = 32;
const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(30);

fn run_server(session_name: String, command: Option<String>) -> Result<()> {
    let socket = prepare_socket_path(&session_name)?;
    let listener = UnixListener::bind(&socket).context("bind rmux server socket")?;
    secure_socket_path(&socket)?;
    listener
        .set_nonblocking(true)
        .context("configure rmux server socket")?;
    let terminal_config = TerminalConfig::load();
    let app = Rmux::new(
        TtySize { cols: 80, rows: 24 },
        terminal_config,
        session_name,
        command.as_deref(),
    )?;
    let app = Arc::new(Mutex::new(app));
    let shutdown = Arc::new(AtomicBool::new(false));
    let active_connections = Arc::new(AtomicUsize::new(0));

    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .context("configure rmux client socket")?;
                let _ = stream.set_read_timeout(Some(CLIENT_READ_TIMEOUT));

                if active_connections.load(Ordering::SeqCst) >= MAX_CLIENT_CONNECTIONS {
                    continue;
                }

                let app = Arc::clone(&app);
                let shutdown = Arc::clone(&shutdown);
                let counter = Arc::clone(&active_connections);
                counter.fetch_add(1, Ordering::SeqCst);
                thread::spawn(move || {
                    let _ = handle_client(&mut stream, app, shutdown);
                    counter.fetch_sub(1, Ordering::SeqCst);
                });
            }
            Err(err) if err.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err).context("accept rmux client"),
        }
    }

    let _ = fs::remove_file(socket);
    Ok(())
}

fn lock_app(app: &Arc<Mutex<Rmux>>) -> std::sync::MutexGuard<'_, Rmux> {
    app.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn handle_client(
    stream: &mut UnixStream,
    app: Arc<Mutex<Rmux>>,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let mut seen_detach_generation = lock_app(&app).detach_generation();

    loop {
        let request = match read_json::<ClientRequest>(stream) {
            Ok(request) => request,
            Err(_) => return Ok(()),
        };

        let pending = {
            let mut app = lock_app(&app);
            let result = match request {
                ClientRequest::Resize(size) => {
                    app.resize(size);
                    app.drain_events();
                    pending_after_drain(&mut app)
                }
                ClientRequest::Key(key) => match app.handle_key(KeyEvent::from(key)) {
                    Ok(()) => {
                        app.drain_events();
                        pending_after_drain(&mut app)
                    }
                    Err(err) => LockedResult::Response(ServerResponse::Error(err.to_string())),
                },
                ClientRequest::Paste(text) => {
                    app.paste_active(text.as_bytes());
                    app.drain_events();
                    pending_without_view(&mut app)
                }
                ClientRequest::Mouse(mouse) => {
                    if app.handle_mouse(mouse) {
                        app.drain_events();
                        pending_after_drain(&mut app)
                    } else {
                        LockedResult::Response(ServerResponse::Noop)
                    }
                }
                ClientRequest::Snapshot => {
                    app.drain_events();
                    pending_after_drain(&mut app)
                }
                ClientRequest::Write(bytes) => {
                    app.write_active(&bytes);
                    app.drain_events();
                    pending_after_drain(&mut app)
                }
                ClientRequest::Command(command) => {
                    app.clear_selection();
                    match apply_rmux_command(&mut app, command) {
                        Ok(()) => {
                            app.drain_events();
                            pending_after_drain(&mut app)
                        }
                        Err(err) => LockedResult::Response(ServerResponse::Error(err.to_string())),
                    }
                }
                ClientRequest::Shutdown => LockedResult::Response(ServerResponse::Shutdown),
            };
            if app.detach_generation() != seen_detach_generation {
                seen_detach_generation = app.detach_generation();
                LockedResult::Response(ServerResponse::Detached)
            } else {
                result
            }
        };

        let response = match pending {
            LockedResult::Response(r) => r,
            LockedResult::View(raw) => ServerResponse::View(Box::new(build_session_view(*raw))),
        };

        if write_json(stream, &response).is_err() {
            return Ok(());
        }
        if matches!(response, ServerResponse::Shutdown) {
            shutdown.store(true, Ordering::SeqCst);
            return Ok(());
        }
        if matches!(response, ServerResponse::Detached) {
            return Ok(());
        }
        if matches!(response, ServerResponse::Error(_)) {
            return Ok(());
        }
    }
}

enum LockedResult {
    Response(ServerResponse),
    View(Box<RawView>),
}

fn pending_after_drain(app: &mut Rmux) -> LockedResult {
    match app.take_exit_reason() {
        Some(ExitReason::Detach) => LockedResult::Response(ServerResponse::Detached),
        Some(ExitReason::Quit) => LockedResult::Response(ServerResponse::Shutdown),
        None => LockedResult::View(Box::new(app.collect_raw_view())),
    }
}

fn pending_without_view(app: &mut Rmux) -> LockedResult {
    match app.take_exit_reason() {
        Some(ExitReason::Detach) => LockedResult::Response(ServerResponse::Detached),
        Some(ExitReason::Quit) => LockedResult::Response(ServerResponse::Shutdown),
        None => LockedResult::Response(ServerResponse::Noop),
    }
}

fn apply_rmux_command(app: &mut Rmux, command: RmuxCommand) -> Result<()> {
    match command {
        RmuxCommand::NewWindow { name, command } => app.new_window(name, command.as_deref()),
        RmuxCommand::SplitWindow { axis, command } => app.split_window(axis, command.as_deref()),
        RmuxCommand::SelectWindow { index } => {
            if app.select_window(index) {
                Ok(())
            } else {
                Err(anyhow!("window {} does not exist", index + 1))
            }
        }
        RmuxCommand::NextWindow => {
            app.next_window();
            Ok(())
        }
        RmuxCommand::PreviousWindow => {
            app.previous_window();
            Ok(())
        }
        RmuxCommand::LastWindow => {
            if app.last_window() {
                Ok(())
            } else {
                Err(anyhow!("no last window"))
            }
        }
        RmuxCommand::SelectPane { index } => {
            if app.select_pane(index) {
                Ok(())
            } else {
                Err(anyhow!("pane {} does not exist", index + 1))
            }
        }
        RmuxCommand::NextPane => {
            app.next_pane();
            Ok(())
        }
        RmuxCommand::PreviousPane => {
            app.previous_pane();
            Ok(())
        }
        RmuxCommand::SelectLayout { axis } => {
            app.select_layout(axis);
            Ok(())
        }
        RmuxCommand::ResizePane { direction, amount } => {
            if app.resize_active_pane(direction, amount) {
                Ok(())
            } else {
                Err(anyhow!("pane cannot be resized in that direction"))
            }
        }
        RmuxCommand::SwapPane { direction } => {
            if app.swap_active_pane(direction) {
                Ok(())
            } else {
                Err(anyhow!("pane cannot be swapped in that direction"))
            }
        }
        RmuxCommand::SwapWindow { direction } => {
            if app.swap_active_window(direction) {
                Ok(())
            } else {
                Err(anyhow!("window cannot be swapped in that direction"))
            }
        }
        RmuxCommand::MoveWindow { index } => {
            if app.move_active_window(index) {
                Ok(())
            } else {
                Err(anyhow!("window {} does not exist", index + 1))
            }
        }
        RmuxCommand::SendPrefix => {
            app.send_prefix();
            Ok(())
        }
        RmuxCommand::BreakPane { name } => {
            if app.break_active_pane(name) {
                Ok(())
            } else {
                Err(anyhow!("last pane cannot be broken into a new window"))
            }
        }
        RmuxCommand::JoinPane {
            source_window,
            source_pane,
        } => {
            if app.join_pane(source_window, source_pane) {
                Ok(())
            } else {
                Err(anyhow!("pane cannot be joined into target window"))
            }
        }
        RmuxCommand::DetachClient => {
            app.detach_clients();
            Ok(())
        }
        RmuxCommand::KillPane { all_other } => {
            if all_other {
                if app.kill_other_panes() {
                    Ok(())
                } else {
                    Err(anyhow!("no other panes to kill"))
                }
            } else if app.kill_active_pane() {
                Ok(())
            } else {
                Err(anyhow!("last pane cannot be killed"))
            }
        }
        RmuxCommand::RespawnPane { command } => app.respawn_active_pane(command.as_deref()),
        RmuxCommand::RespawnWindow { command } => app.respawn_active_window(command.as_deref()),
        RmuxCommand::SetSynchronizePanes { enabled } => {
            app.set_synchronize_panes(enabled);
            Ok(())
        }
        RmuxCommand::SetDefaultCommand { command } => {
            app.set_default_command(command);
            Ok(())
        }
        RmuxCommand::SetPrefixKey { key } => {
            app.set_prefix_key(key);
            Ok(())
        }
        RmuxCommand::BindKey { key, command } => {
            app.bind_key(key, command);
            Ok(())
        }
        RmuxCommand::KillWindow => {
            if app.kill_active_window() {
                Ok(())
            } else {
                Err(anyhow!("last window cannot be killed"))
            }
        }
        RmuxCommand::RenameWindow { name } => {
            app.rename_active_window(name);
            Ok(())
        }
        RmuxCommand::RenameSession { name } => {
            app.rename_session(name);
            Ok(())
        }
    }
}

fn list_sessions(format: Option<&str>) -> Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    for name in list_session_names()? {
        if let Some(format) = format {
            let mut client = connect_session(&name)?;
            let view = client.request(ClientRequest::Snapshot)?.into_view()?;
            write_stdout_line(&mut out, format_args!("{}", render_format(&view, format)?))?;
        } else {
            write_stdout_line(&mut out, format_args!("{name}"))?;
        }
    }
    Ok(())
}

fn has_session(session: &str) -> bool {
    connect_session(session).is_ok()
}

fn wait_for(target: CliTarget, pattern: Option<&str>, timeout_ms: u64) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let matched = match pattern {
            Some(pattern) => target_capture_contains(&target, pattern).unwrap_or(false),
            None => has_session(&target.session),
        };
        if matched {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return match pattern {
                Some(pattern) => Err(anyhow!(
                    "timed out waiting for target {} to contain {pattern:?}",
                    target.session
                )),
                None => Err(anyhow!("timed out waiting for session {}", target.session)),
            };
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn target_capture_contains(target: &CliTarget, pattern: &str) -> Result<bool> {
    apply_target(target)?;
    let mut client = connect_session(&target.session)?;
    let view = client.request(ClientRequest::Snapshot)?.into_view()?;
    let Some(pane) = view.panes.get(view.active_pane) else {
        return Ok(false);
    };
    Ok(pane.lines.iter().any(|line| line.contains(pattern)))
}

fn write_stdout_line(out: &mut impl Write, args: std::fmt::Arguments<'_>) -> Result<()> {
    match out.write_fmt(args).and_then(|_| out.write_all(b"\n")) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == ErrorKind::BrokenPipe => Ok(()),
        Err(err) => Err(err).context("write rmux output"),
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode().with_context(
            || "enter raw terminal mode; rmux must be run from an interactive terminal",
        )?;
        match execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste
        ) {
            Ok(()) => Ok(Self),
            Err(err) => {
                let _ = execute!(
                    io::stdout(),
                    DisableBracketedPaste,
                    DisableMouseCapture,
                    LeaveAlternateScreen
                );
                let _ = disable_raw_mode();
                Err(err).context("enter alternate terminal screen")
            }
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

fn draw_session_view(
    terminal: &mut RatTerminal<CrosstermBackend<io::Stdout>>,
    view: &protocol::SessionView,
) -> Result<()> {
    if let Some(text) = view.clipboard_text.as_deref() {
        copy_to_system_clipboard(text)?;
    }
    match terminal.draw(|frame| draw_view(frame, view)) {
        Ok(_) => Ok(()),
        Err(err) => Err(err).context("draw rmux terminal"),
    }
}

#[cfg(target_os = "macos")]
fn copy_to_system_clipboard(text: &str) -> Result<()> {
    let mut child = ProcessCommand::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("start pbcopy")?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("pbcopy stdin unavailable"))?
        .write_all(text.as_bytes())
        .context("write selection to pbcopy")?;
    let status = child.wait().context("wait for pbcopy")?;
    if !status.success() {
        return Err(anyhow!("pbcopy exited with {status}"));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn copy_to_system_clipboard(text: &str) -> Result<()> {
    let mut stdout = io::stdout();
    stdout
        .write_all(osc52_clipboard_sequence(text).as_bytes())
        .context("write OSC 52 clipboard sequence")?;
    stdout.flush().context("flush OSC 52 clipboard sequence")
}

#[cfg(any(not(target_os = "macos"), test))]
fn osc52_clipboard_sequence(text: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::STANDARD};

    format!("\x1b]52;c;{}\x07", STANDARD.encode(text.as_bytes()))
}

fn is_terminal_output_closed(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        ErrorKind::BrokenPipe | ErrorKind::ConnectionReset | ErrorKind::NotConnected
    )
}

fn terminal_input_ready(timeout: Duration) -> Result<bool> {
    match event::poll(timeout) {
        Ok(ready) => Ok(ready),
        Err(err) if is_terminal_output_closed(&err) => Ok(false),
        Err(err) => Err(err).context("poll terminal input"),
    }
}

fn read_terminal_event() -> Result<Option<Event>> {
    match event::read() {
        Ok(event) => Ok(Some(event)),
        Err(err) if is_terminal_output_closed(&err) => Ok(None),
        Err(err) => Err(err).context("read terminal input"),
    }
}

fn available_terminal_size() -> Result<TtySize> {
    let (cols, rows) = terminal_size().context("read terminal size")?;
    Ok(TtySize { cols, rows })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::layout::Rect;

    use crate::{
        model::{PrefixCommand, encode_key, prefix_command},
        protocol::{ResizeDirection, SplitAxis, WireMouse, WireMouseButton, WireMouseKind},
        render::compute_pane_rects,
    };

    fn monotonic_test_id() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    #[test]
    fn osc52_clipboard_payload_is_base64_encoded() {
        assert_eq!(osc52_clipboard_sequence("hello"), "\x1b]52;c;aGVsbG8=\x07");
    }

    #[test]
    fn paste_event_becomes_one_bulk_request() {
        let payload = "first line\nsecond line 🦀";
        let request = terminal_event_request(Event::Paste(payload.to_string()));

        assert!(
            matches!(request, Some(ClientRequest::Paste(text)) if text == payload),
            "a complete paste should stay in one request"
        );
    }

    #[test]
    fn paste_falls_back_to_bulk_write_for_older_servers() {
        let session = format!(
            "rmux-test-paste-fallback-{}-{}",
            std::process::id(),
            monotonic_test_id()
        );
        let socket = prepare_socket_path(&session).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        secure_socket_path(&socket).unwrap();
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut unsupported, _) = listener.accept().unwrap();
                let request = read_json::<serde_json::Value>(&mut unsupported).unwrap();
                assert_eq!(request, serde_json::json!({ "Paste": "first" }));
            }

            let (mut legacy, _) = listener.accept().unwrap();
            for expected in [b"first".as_slice(), b"second".as_slice()] {
                let request = read_json::<ClientRequest>(&mut legacy).unwrap();
                assert!(
                    matches!(request, ClientRequest::Write(bytes) if bytes == expected),
                    "fallback should keep each paste in one legacy Write request"
                );
                write_json(&mut legacy, &ServerResponse::Noop).unwrap();
            }
            let _ = fs::remove_file(server_socket);
        });

        let mut client = connect_session(&session).unwrap();
        let mut paste_request_supported = true;
        assert!(matches!(
            request_paste_with_fallback(
                &mut client,
                &session,
                "first".to_string(),
                &mut paste_request_supported,
            )
            .unwrap(),
            ServerResponse::Noop
        ));
        assert!(!paste_request_supported);
        assert!(matches!(
            request_paste_with_fallback(
                &mut client,
                &session,
                "second".to_string(),
                &mut paste_request_supported,
            )
            .unwrap(),
            ServerResponse::Noop
        ));

        server.join().unwrap();
        let _ = fs::remove_file(socket);
    }

    #[test]
    fn paste_keeps_new_protocol_after_a_stale_connection() {
        let session = format!(
            "rmux-test-paste-reconnect-{}-{}",
            std::process::id(),
            monotonic_test_id()
        );
        let socket = prepare_socket_path(&session).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        secure_socket_path(&socket).unwrap();
        let server_socket = socket.clone();
        let server = thread::spawn(move || {
            let (stale, _) = listener.accept().unwrap();
            drop(stale);

            let (mut fresh, _) = listener.accept().unwrap();
            let request = read_json::<ClientRequest>(&mut fresh).unwrap();
            assert!(matches!(request, ClientRequest::Paste(text) if text == "fresh"));
            write_json(&mut fresh, &ServerResponse::Noop).unwrap();
            let _ = fs::remove_file(server_socket);
        });

        let mut client = connect_session(&session).unwrap();
        let mut paste_request_supported = true;
        assert!(matches!(
            request_paste_with_fallback(
                &mut client,
                &session,
                "fresh".to_string(),
                &mut paste_request_supported,
            )
            .unwrap(),
            ServerResponse::Noop
        ));
        assert!(paste_request_supported);

        server.join().unwrap();
        let _ = fs::remove_file(socket);
    }

    #[test]
    fn resize_requests_are_deduplicated() {
        let mut last_size = TtySize { cols: 80, rows: 24 };
        let request = resize_request(
            TtySize {
                cols: 120,
                rows: 40,
            },
            &mut last_size,
        );

        assert!(matches!(
            request,
            Some(ClientRequest::Resize(TtySize {
                cols: 120,
                rows: 40
            }))
        ));
        assert!(
            resize_request(
                TtySize {
                    cols: 120,
                    rows: 40,
                },
                &mut last_size,
            )
            .is_none()
        );
    }

    #[test]
    fn horizontal_layout_uses_full_width() {
        let area = Rect::new(0, 0, 101, 20);
        let rects = compute_pane_rects(area, 3, SplitAxis::Horizontal, &[]);

        assert_eq!(rects.len(), 3);
        assert_eq!(rects[0].width, 33);
        assert_eq!(rects[1].width, 34);
        assert_eq!(rects[2].width, 34);
        assert_eq!(rects[2].x + rects[2].width, 101);
    }

    #[test]
    fn vertical_layout_uses_full_height() {
        let area = Rect::new(0, 0, 80, 25);
        let rects = compute_pane_rects(area, 2, SplitAxis::Vertical, &[]);

        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].height, 12);
        assert_eq!(rects[1].height, 13);
        assert_eq!(rects[1].y + rects[1].height, 25);
    }

    #[test]
    fn weighted_layout_respects_pane_weights() {
        let area = Rect::new(0, 0, 100, 20);
        let rects = compute_pane_rects(area, 2, SplitAxis::Horizontal, &[125, 75]);

        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].width, 62);
        assert_eq!(rects[1].width, 38);
        assert_eq!(rects[1].x + rects[1].width, 100);
    }

    #[test]
    fn prefix_key_does_not_send_to_terminal() {
        let key = KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL);
        assert_eq!(encode_key(key), vec![2]);
    }

    #[test]
    fn bound_prefix_key_runs_command() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.bind_key('v', BoundCommand::SplitHorizontal);
        app.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL))
            .unwrap();
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE))
            .unwrap();

        assert_eq!(app.session_view().panes.len(), 2);
    }

    #[test]
    fn terminal_config_working_dir_is_used_for_spawned_panes() {
        let workdir = std::env::temp_dir().join(format!(
            "rmux-test-cwd-{}-{}",
            std::process::id(),
            monotonic_test_id()
        ));
        fs::create_dir_all(&workdir).unwrap();
        let terminal_config = TerminalConfig::for_test(Some(workdir.to_string_lossy().to_string()));
        let mut app = Rmux::new(
            TtySize {
                cols: 100,
                rows: 24,
            },
            terminal_config,
            "test",
            Some("pwd; sleep 1"),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            app.drain_events();
            let view = app.session_view();
            if view.panes[0]
                .lines
                .iter()
                .any(|line| line.contains(&workdir.to_string_lossy().to_string()))
            {
                let _ = fs::remove_dir_all(&workdir);
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        let _ = fs::remove_dir_all(&workdir);
        panic!("configured working_dir was not used for pane spawn");
    }

    #[test]
    fn terminal_split_shortcuts_create_panes() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL))
            .unwrap();
        let view = app.session_view();
        assert_eq!(view.panes.len(), 2);
        assert_eq!(view.split_axis, SplitAxis::Horizontal);
        assert_eq!(view.panes[0].x + view.panes[0].width, view.panes[1].x);

        app.handle_key(KeyEvent::new(
            KeyCode::Char('D'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .unwrap();
        let view = app.session_view();
        assert_eq!(view.panes.len(), 3);
        assert_eq!(view.split_axis, SplitAxis::Horizontal);
        assert_eq!(view.panes[1].x, view.panes[2].x);
        assert_eq!(view.panes[1].width, view.panes[2].width);
        assert!(view.panes[2].y > view.panes[1].y);
        assert_eq!(
            view.panes[0].height,
            view.panes[1].height + view.panes[2].height
        );
    }

    #[test]
    fn super_d_split_shortcut_is_supported_for_macos_terminals() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::SUPER))
            .unwrap();

        assert_eq!(app.session_view().panes.len(), 2);
    }

    #[test]
    fn ctrl_t_creates_new_window_not_pane() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL))
            .unwrap();

        let view = app.session_view();
        assert_eq!(view.windows.len(), 2);
        assert_eq!(view.active_window, 1);
        assert_eq!(view.panes.len(), 1);
    }

    #[test]
    fn ctrl_number_selects_window() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.new_window(None, Some("cat")).unwrap();
        app.new_window(None, Some("cat")).unwrap();
        assert_eq!(app.session_view().active_window, 2);

        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL))
            .unwrap();

        assert_eq!(app.session_view().active_window, 0);
    }

    #[test]
    fn ctrl_shift_number_selects_window_when_terminal_sends_shifted_digit() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.new_window(None, Some("cat")).unwrap();
        app.new_window(None, Some("cat")).unwrap();
        assert_eq!(app.session_view().active_window, 2);

        app.handle_key(KeyEvent::new(
            KeyCode::Char('!'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ))
        .unwrap();

        assert_eq!(app.session_view().active_window, 0);
    }

    #[test]
    fn alt_number_selects_window() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.new_window(None, Some("cat")).unwrap();
        app.new_window(None, Some("cat")).unwrap();
        assert_eq!(app.session_view().active_window, 2);

        app.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::ALT))
            .unwrap();

        assert_eq!(app.session_view().active_window, 0);

        app.handle_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::ALT))
            .unwrap();

        assert_eq!(app.session_view().active_window, 2);
    }

    #[test]
    fn ctrl_w_closes_active_pane() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.split_window(SplitAxis::Horizontal, Some("cat"))
            .unwrap();

        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL))
            .unwrap();

        assert_eq!(app.session_view().panes.len(), 1);
    }

    #[test]
    fn split_still_tiles_after_killing_a_pane() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();

        app.split_window(SplitAxis::Horizontal, Some("cat"))
            .unwrap();
        assert!(app.kill_active_pane());
        app.split_window(SplitAxis::Horizontal, Some("cat"))
            .unwrap();

        let view = app.session_view();
        assert_eq!(view.panes.len(), 2);
        assert_eq!(view.panes[0].x + view.panes[0].width, view.panes[1].x);
        assert!(view.panes[0].width < 80 && view.panes[1].width < 80);
    }

    #[test]
    fn killing_middle_pane_keeps_remaining_layout_tiled() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 90, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();

        app.split_window(SplitAxis::Horizontal, Some("cat"))
            .unwrap();
        app.split_window(SplitAxis::Horizontal, Some("cat"))
            .unwrap();
        assert_eq!(app.session_view().panes.len(), 3);

        app.previous_pane();
        assert!(app.kill_active_pane());

        let view = app.session_view();
        assert_eq!(view.panes.len(), 2);
        let total: u16 = view.panes.iter().map(|pane| pane.width).sum();
        assert_eq!(total, 90, "remaining panes must tile the full width");
        assert_eq!(view.panes[0].x + view.panes[0].width, view.panes[1].x);
    }

    #[test]
    fn exited_child_closes_its_pane() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.split_window(SplitAxis::Horizontal, Some("printf done"))
            .unwrap();
        assert_eq!(app.session_view().panes.len(), 2);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.drain_events();
            if app.session_view().panes.len() == 1 {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!("exited child pane did not close");
    }

    #[test]
    fn exited_last_pane_requests_shutdown() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("printf done"),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            app.drain_events();
            if app.take_exit_reason() == Some(ExitReason::Quit) {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!("exited final pane did not request shutdown");
    }

    #[test]
    fn mouse_click_selects_pane() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.split_window(SplitAxis::Horizontal, Some("cat"))
            .unwrap();
        assert!(app.select_pane(0));

        app.handle_mouse(WireMouse {
            col: 60,
            row: 5,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 0,
        });
        assert_eq!(app.session_view().active_pane, 1);
        assert!(!app.select_pane_at(60, 23));
    }

    #[test]
    fn status_bar_click_selects_window_tab() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();
        app.new_window(None, Some("cat")).unwrap();
        assert_eq!(app.session_view().active_window, 1);

        // Status row layout: " test " (6 cols) + " 1:win1#1 " (10 cols) + " 2:win2#2 ".
        assert!(app.handle_mouse(WireMouse {
            col: 7,
            row: 23,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 0,
        }));
        assert_eq!(app.session_view().active_window, 0);

        assert!(app.handle_mouse(WireMouse {
            col: 17,
            row: 23,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 0,
        }));
        assert_eq!(app.session_view().active_window, 1);

        // Clicks past the last tab do nothing.
        assert!(!app.handle_mouse(WireMouse {
            col: 60,
            row: 23,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 0,
        }));
    }

    #[test]
    fn mouse_move_without_terminal_reporting_is_ignored() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("cat"),
        )
        .unwrap();

        assert!(!app.handle_mouse(WireMouse {
            col: 4,
            row: 2,
            kind: WireMouseKind::Moved,
            modifiers: 0,
        }));
    }

    #[test]
    fn mouse_drag_selects_and_copies_visible_text() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("printf 'hello world'; sleep 2"),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            app.drain_events();
            if app.session_view().panes[0]
                .lines
                .iter()
                .any(|line| line.contains("hello world"))
            {
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }

        assert!(app.handle_mouse(WireMouse {
            col: 0,
            row: 0,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 0,
        }));
        assert!(app.handle_mouse(WireMouse {
            col: 4,
            row: 0,
            kind: WireMouseKind::Drag(WireMouseButton::Left),
            modifiers: 0,
        }));
        assert!(app.handle_mouse(WireMouse {
            col: 4,
            row: 0,
            kind: WireMouseKind::Up(WireMouseButton::Left),
            modifiers: 0,
        }));

        let view = app.session_view();
        let selection = view.panes[0].selection.expect("selection should persist");
        assert_eq!(selection.start, protocol::CursorView { col: 0, row: 0 });
        assert_eq!(selection.end, protocol::CursorView { col: 4, row: 0 });
        assert_eq!(view.clipboard_text.as_deref(), Some("hello"));
        assert!(app.session_view().clipboard_text.is_none());
    }

    #[test]
    fn mouse_wheel_scrolls_terminal_history() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("i=1; while [ \"$i\" -le 80 ]; do echo \"line-$i\"; i=$((i+1)); done; sleep 2"),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        let bottom_view = loop {
            app.drain_events();
            let view = app.session_view();
            if view.panes[0].history_size > 0 {
                break view;
            }
            assert!(
                Instant::now() < deadline,
                "terminal did not build scrollback"
            );
            thread::sleep(Duration::from_millis(25));
        };
        assert_eq!(bottom_view.panes[0].scroll_offset, 0);

        assert!(app.handle_mouse(WireMouse {
            col: 2,
            row: 2,
            kind: WireMouseKind::ScrollUp,
            modifiers: 0,
        }));
        let scrolled_up = app.session_view();
        assert!(scrolled_up.panes[0].scroll_offset > 0);
        assert_ne!(scrolled_up.panes[0].lines, bottom_view.panes[0].lines);

        let offset = scrolled_up.panes[0].scroll_offset;
        assert!(app.handle_mouse(WireMouse {
            col: 2,
            row: 2,
            kind: WireMouseKind::ScrollDown,
            modifiers: 0,
        }));
        assert!(app.session_view().panes[0].scroll_offset < offset);

        assert!(app.handle_mouse(WireMouse {
            col: 2,
            row: 2,
            kind: WireMouseKind::ScrollUp,
            modifiers: 0,
        }));
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(app.session_view().panes[0].scroll_offset, 0);
    }

    #[test]
    fn paste_honors_child_bracketed_paste_mode() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("printf '\\033[?2004hPASTE_READY'; cat -v"),
        )
        .unwrap();

        let ready_deadline = Instant::now() + Duration::from_secs(1);
        let mut ready = false;
        while Instant::now() < ready_deadline {
            app.drain_events();
            if app.session_view().panes[0]
                .lines
                .iter()
                .any(|line| line.contains("PASTE_READY"))
            {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(ready, "child did not enable bracketed paste mode");

        app.paste_active(b"left\x1b[201~middle\x1b[200~right");

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            app.drain_events();
            let view = app.session_view();
            if view.panes[0]
                .lines
                .iter()
                .any(|line| line.contains("^[[200~leftmiddleright^[[201~"))
            {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!("paste was not sent as one sanitized bracketed payload");
    }

    #[test]
    fn paste_stays_raw_when_child_bracketed_paste_is_disabled() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("stty -echo; printf 'RAW_READY'; cat -v"),
        )
        .unwrap();

        let ready_deadline = Instant::now() + Duration::from_secs(1);
        let mut ready = false;
        while Instant::now() < ready_deadline {
            app.drain_events();
            if app.session_view().panes[0]
                .lines
                .iter()
                .any(|line| line.contains("RAW_READY"))
            {
                ready = true;
                break;
            }
            thread::sleep(Duration::from_millis(25));
        }
        assert!(ready, "raw-paste child did not become ready");

        app.paste_active(b"raw-paste\n");

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            app.drain_events();
            let view = app.session_view();
            if view.panes[0]
                .lines
                .iter()
                .any(|line| line.contains("raw-paste"))
            {
                assert!(
                    view.panes[0]
                        .lines
                        .iter()
                        .all(|line| !line.contains("[200~") && !line.contains("[201~"))
                );
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!("raw paste did not reach the child");
    }

    #[test]
    fn mouse_click_forwards_when_terminal_mouse_reporting_is_enabled() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 80, rows: 24 },
            terminal_config,
            "test",
            Some("printf '\\033[?1000h\\033[?1006h'; cat -v"),
        )
        .unwrap();

        let ready_deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < ready_deadline {
            app.drain_events();
            thread::sleep(Duration::from_millis(25));
        }

        assert!(app.handle_mouse(WireMouse {
            col: 4,
            row: 2,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 1,
        }));
        assert!(app.handle_mouse(WireMouse {
            col: 6,
            row: 2,
            kind: WireMouseKind::Drag(WireMouseButton::Left),
            modifiers: 1,
        }));
        assert!(app.handle_mouse(WireMouse {
            col: 6,
            row: 2,
            kind: WireMouseKind::Up(WireMouseButton::Left),
            modifiers: 0,
        }));
        assert!(app.session_view().panes[0].selection.is_some());

        app.handle_mouse(WireMouse {
            col: 4,
            row: 2,
            kind: WireMouseKind::Down(WireMouseButton::Left),
            modifiers: 0,
        });
        assert!(app.session_view().panes[0].selection.is_none());

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            app.drain_events();
            let view = app.session_view();
            if view.panes[0]
                .lines
                .iter()
                .any(|line| line.contains("[<0;5;3M"))
            {
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!("mouse report packet was not forwarded to the pane");
    }

    #[test]
    fn session_view_preserves_ansi_cell_colors() {
        let terminal_config = TerminalConfig::load();
        let mut app = Rmux::new(
            TtySize { cols: 40, rows: 10 },
            terminal_config,
            "test",
            Some("printf '\\033[31mred\\033[0m\\n'; sleep 1"),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            app.drain_events();
            let view = app.session_view();
            if let Some(red_cell) = view.panes[0].cells.iter().find(|cell| cell.ch == 'r') {
                assert!(
                    red_cell.fg.r > red_cell.fg.g && red_cell.fg.r > red_cell.fg.b,
                    "expected red ANSI foreground, got {:?}",
                    red_cell.fg
                );
                return;
            }
            thread::sleep(Duration::from_millis(25));
        }

        panic!("red ANSI cell did not appear in session view");
    }

    #[test]
    fn parses_new_session_command() {
        let args = Args::parse_from([
            "rmux",
            "new-session",
            "--session",
            "work",
            "--command",
            "zsh",
            "--detached",
            "--attach",
            "--print",
            "--format",
            "#{session_name}",
        ]);

        match args.command {
            Some(Command::NewSession {
                session,
                command,
                detached,
                attach,
                print,
                format,
            }) => {
                assert_eq!(session.as_deref(), Some("work"));
                assert_eq!(command.as_deref(), Some("zsh"));
                assert!(detached);
                assert!(attach);
                assert!(print);
                assert_eq!(format, "#{session_name}");
            }
            other => panic!("expected new-session, got {other:?}"),
        }
    }

    #[test]
    fn parses_attach_target() {
        let args = Args::parse_from(["rmux", "attach", "--target", "work:logs.1"]);

        match args.command {
            Some(Command::Attach { target }) => assert_eq!(target.as_deref(), Some("work:logs.1")),
            other => panic!("expected attach, got {other:?}"),
        }
    }

    #[test]
    fn parses_list_sessions() {
        let args = Args::parse_from(["rmux", "list-sessions"]);

        assert!(matches!(
            args.command,
            Some(Command::ListSessions { format }) if format.is_none()
        ));

        let args = Args::parse_from([
            "rmux",
            "list-sessions",
            "--format",
            "#{session_name}:#{window_count}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::ListSessions { format })
                if format.as_deref() == Some("#{session_name}:#{window_count}")
        ));

        let args = Args::parse_from(["rmux", "has-session", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::HasSession { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from([
            "rmux",
            "wait-for",
            "--target",
            "work:1.2",
            "--timeout-ms",
            "25",
            "ready",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::WaitFor {
                target,
                pattern,
                timeout_ms
            }) if target.as_deref() == Some("work:1.2")
                && pattern.as_deref() == Some("ready")
                && timeout_ms == 25
        ));
    }

    #[test]
    fn parses_send_keys_and_capture_pane() {
        let args = Args::parse_from([
            "rmux",
            "send-keys",
            "--target",
            "work",
            "--literal",
            "echo hi",
            "Enter",
        ]);
        match args.command {
            Some(Command::SendKeys {
                target,
                literal,
                keys,
            }) => {
                assert_eq!(target.as_deref(), Some("work"));
                assert!(literal);
                assert_eq!(keys, ["echo hi", "Enter"]);
            }
            other => panic!("expected send-keys, got {other:?}"),
        }

        let args = Args::parse_from(["rmux", "capture-pane", "--target", "work", "--all-panes"]);
        assert!(matches!(
            args.command,
            Some(Command::CapturePane { target, all_panes })
                if target.as_deref() == Some("work") && all_panes
        ));

        let args = Args::parse_from([
            "rmux",
            "display-message",
            "--target",
            "work:1.2",
            "--format",
            "#{session_name}:#{window_index}.#{pane_index}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::DisplayMessage { target, format })
                if target.as_deref() == Some("work:1.2")
                    && format == "#{session_name}:#{window_index}.#{pane_index}"
        ));

        let args = Args::parse_from(["rmux", "send-prefix", "--target", "work:1.2"]);
        assert!(matches!(
            args.command,
            Some(Command::SendPrefix { target }) if target.as_deref() == Some("work:1.2")
        ));
    }

    #[test]
    fn encodes_send_key_tokens() {
        let keys = [
            "echo".to_string(),
            "Space".to_string(),
            "ok".to_string(),
            "Enter".to_string(),
        ];
        assert_eq!(encode_key_tokens(&keys).unwrap(), b"echo ok\r");

        let keys = ["C-c".to_string(), "Tab".to_string(), "Left".to_string()];
        assert_eq!(encode_key_tokens(&keys).unwrap(), b"\x03\t\x1b[D");

        let keys = ["Enter".to_string(), "Tab".to_string()];
        assert_eq!(encode_send_keys(&keys, true).unwrap(), b"EnterTab");
    }

    #[test]
    fn parses_cli_targets() {
        assert_eq!(
            parse_target("work", "default").unwrap(),
            CliTarget {
                session: "work".to_string(),
                window: None,
                pane: None
            }
        );
        assert_eq!(
            parse_target("work:2.3", "default").unwrap(),
            CliTarget {
                session: "work".to_string(),
                window: Some(WindowSelector::Index(1)),
                pane: Some(PaneSelector::Index(2))
            }
        );
        assert_eq!(
            parse_target("work:logs.1", "default").unwrap(),
            CliTarget {
                session: "work".to_string(),
                window: Some(WindowSelector::Name("logs".to_string())),
                pane: Some(PaneSelector::Index(0))
            }
        );
        assert_eq!(
            parse_target("work:#7.%42", "default").unwrap(),
            CliTarget {
                session: "work".to_string(),
                window: Some(WindowSelector::Id(7)),
                pane: Some(PaneSelector::Id(42))
            }
        );
        assert_eq!(
            parse_target(":1.2", "default").unwrap(),
            CliTarget {
                session: "default".to_string(),
                window: Some(WindowSelector::Index(0)),
                pane: Some(PaneSelector::Index(1))
            }
        );
        assert!(parse_target("work:0", "default").is_err());
    }

    #[test]
    fn renders_display_format() {
        let view = protocol::SessionView {
            session: "work".to_string(),
            default_command: Some("zsh".to_string()),
            prefix_key: "C-a".to_string(),
            key_bindings: Vec::new(),
            windows: vec![protocol::WindowTabView {
                id: 7,
                name: "logs".to_string(),
                synchronize_panes: true,
            }],
            active_window: 0,
            panes: vec![protocol::PaneView {
                id: 42,
                x: 0,
                y: 0,
                width: 80,
                height: 24,
                cols: 80,
                rows: 24,
                cells: Vec::new(),
                cursor: None,
                scroll_offset: 0,
                history_size: 0,
                selection: None,
                lines: Vec::new(),
            }],
            active_pane: 0,
            split_axis: SplitAxis::Horizontal,
            pane_weights: vec![125],
            prefix: false,
            message: None,
            clipboard_text: None,
        };

        assert_eq!(
            render_format(
                &view,
                "#{session_name}:#{default_command}:#{prefix_key}:#{window_index}:#{window_name}:#{window_active}:#{pane_index}:#{pane_id}:#{pane_active}:#{pane_width}x#{pane_height}:#{window_layout}:#{pane_weight}:#{synchronize_panes}:#{pane_weights}"
            )
            .unwrap(),
            "work:zsh:C-a:1:logs:1:1:42:1:80x24:even-horizontal:125:1:125"
        );
    }

    #[test]
    fn parses_source_file_lines() {
        assert_eq!(parse_source_line("").unwrap(), None);
        assert_eq!(parse_source_line("   # comment").unwrap(), None);
        assert_eq!(
            parse_source_line("rmux send-keys -t work \"echo hello\" Enter").unwrap(),
            Some(vec![
                "send-keys".to_string(),
                "-t".to_string(),
                "work".to_string(),
                "echo hello".to_string(),
                "Enter".to_string(),
            ])
        );
        assert_eq!(
            parse_source_line("new-window -t work -n logs -c 'printf ready'").unwrap(),
            Some(vec![
                "new-window".to_string(),
                "-t".to_string(),
                "work".to_string(),
                "-n".to_string(),
                "logs".to_string(),
                "-c".to_string(),
                "printf ready".to_string(),
            ])
        );
        assert_eq!(
            parse_source_line("send-keys -t work escaped\\ space Enter").unwrap(),
            Some(vec![
                "send-keys".to_string(),
                "-t".to_string(),
                "work".to_string(),
                "escaped space".to_string(),
                "Enter".to_string(),
            ])
        );
        assert!(parse_source_line("send-keys \"oops").is_err());
        assert!(parse_source_line("send-keys trailing\\").is_err());
    }

    #[test]
    fn parses_scripted_tmux_commands() {
        let args = Args::parse_from([
            "rmux",
            "new-window",
            "--target",
            "work",
            "--name",
            "logs",
            "--command",
            "tail -f /tmp/app.log",
            "--print",
            "--format",
            "#{window_id}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::NewWindow {
                target,
                name,
                command,
                print,
                format
            })
                if target.as_deref() == Some("work")
                    && name.as_deref() == Some("logs")
                    && command.as_deref() == Some("tail -f /tmp/app.log")
                    && print
                    && format == "#{window_id}"
        ));

        let args = Args::parse_from([
            "rmux",
            "split-window",
            "--target",
            "work",
            "--horizontal",
            "--command",
            "top",
            "--print",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::SplitWindow {
                target,
                horizontal,
                command,
                print,
                format
            })
                if target.as_deref() == Some("work")
                    && horizontal
                    && command.as_deref() == Some("top")
                    && print
                    && format == DEFAULT_CREATION_FORMAT
        ));

        let args = Args::parse_from(["rmux", "select-window", "--target", "work", "2"]);
        assert!(matches!(
            args.command,
            Some(Command::SelectWindow { target, index })
                if target.as_deref() == Some("work") && index == 2
        ));

        let args = Args::parse_from(["rmux", "next-window", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::NextWindow { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "previous-window", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::PreviousWindow { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "last-window", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::LastWindow { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "swap-window", "--target", "work", "-U"]);
        assert!(matches!(
            args.command,
            Some(Command::SwapWindow {
                target,
                previous,
                ..
            }) if target.as_deref() == Some("work") && previous
        ));

        let args = Args::parse_from(["rmux", "move-window", "--target", "work", "1"]);
        assert!(matches!(
            args.command,
            Some(Command::MoveWindow { target, index })
                if target.as_deref() == Some("work") && index == 1
        ));

        let args = Args::parse_from([
            "rmux",
            "break-pane",
            "--target",
            "work:1.2",
            "--name",
            "broken",
            "--print",
            "--format",
            "#{window_name}:#{pane_id}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::BreakPane {
                target,
                name,
                print,
                format
            }) if target.as_deref() == Some("work:1.2")
                && name.as_deref() == Some("broken")
                && print
                && format == "#{window_name}:#{pane_id}"
        ));

        let args = Args::parse_from([
            "rmux",
            "join-pane",
            "--source",
            "work:broken.1",
            "--target",
            "work:1",
            "--print",
            "--format",
            "#{window_index}:#{pane_id}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::JoinPane {
                source,
                target,
                print,
                format
            }) if source == "work:broken.1"
                && target.as_deref() == Some("work:1")
                && print
                && format == "#{window_index}:#{pane_id}"
        ));

        let args = Args::parse_from(["rmux", "select-pane", "--target", "work", "2"]);
        assert!(matches!(
            args.command,
            Some(Command::SelectPane { target, index })
                if target.as_deref() == Some("work") && index == 2
        ));

        let args = Args::parse_from(["rmux", "next-pane", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::NextPane { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "previous-pane", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::PreviousPane { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "select-layout", "--target", "work", "even-vertical"]);
        assert!(matches!(
            args.command,
            Some(Command::SelectLayout { target, layout })
                if target.as_deref() == Some("work")
                    && layout == "even-vertical"
                    && parse_layout(&layout).unwrap() == SplitAxis::Vertical
        ));
        assert!(parse_layout("tiled").is_err());

        let args = Args::parse_from(["rmux", "resize-pane", "--target", "work", "-R", "5"]);
        assert!(matches!(
            args.command,
            Some(Command::ResizePane {
                target,
                right,
                amount,
                ..
            }) if target.as_deref() == Some("work")
                && right
                && amount == 5
        ));
        assert_eq!(
            parse_resize_direction(false, true, false, false).unwrap(),
            ResizeDirection::Right
        );
        assert!(parse_resize_direction(false, false, false, false).is_err());
        assert!(parse_resize_direction(true, true, false, false).is_err());

        let args = Args::parse_from(["rmux", "swap-pane", "--target", "work", "-D"]);
        assert!(matches!(
            args.command,
            Some(Command::SwapPane { target, next, .. })
                if target.as_deref() == Some("work")
                    && next
        ));
        assert_eq!(
            parse_swap_direction(true, false).unwrap(),
            SwapDirection::Previous
        );
        assert!(parse_swap_direction(false, false).is_err());
        assert!(parse_swap_direction(true, true).is_err());

        let args = Args::parse_from([
            "rmux",
            "bind-key",
            "--target",
            "work",
            "v",
            "split-window",
            "--horizontal",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::BindKey {
                target,
                key,
                command
            }) if target.as_deref() == Some("work")
                && key == 'v'
                && command == ["split-window", "--horizontal"]
                && parse_bound_command(&command).unwrap() == BoundCommand::SplitHorizontal
        ));
        assert!(parse_bound_command(&["not-real".to_string()]).is_err());

        let args = Args::parse_from(["rmux", "list-keys", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::ListKeys { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "kill-pane", "--target", "work", "--all-other"]);
        assert!(matches!(
            args.command,
            Some(Command::KillPane { target, all_other })
                if target.as_deref() == Some("work") && all_other
        ));

        let args = Args::parse_from([
            "rmux",
            "respawn-pane",
            "--target",
            "work:1.1",
            "--command",
            "zsh",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::RespawnPane { target, command })
                if target.as_deref() == Some("work:1.1")
                    && command.as_deref() == Some("zsh")
        ));

        let args = Args::parse_from([
            "rmux",
            "respawn-window",
            "--target",
            "work:logs",
            "--command",
            "zsh",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::RespawnWindow { target, command })
                if target.as_deref() == Some("work:logs")
                    && command.as_deref() == Some("zsh")
        ));

        let args = Args::parse_from(["rmux", "source-file", "/tmp/rmux.conf"]);
        assert!(matches!(
            args.command,
            Some(Command::SourceFile { path }) if path == "/tmp/rmux.conf"
        ));

        let args = Args::parse_from([
            "rmux",
            "list-panes",
            "--target",
            "work",
            "--format",
            "#{pane_index}:#{pane_id}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::ListPanes { target, format })
                if target.as_deref() == Some("work")
                    && format.as_deref() == Some("#{pane_index}:#{pane_id}")
        ));

        let args = Args::parse_from([
            "rmux",
            "list-windows",
            "--target",
            "work",
            "--format",
            "#{window_index}:#{window_name}",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::ListWindows { target, format })
                if target.as_deref() == Some("work")
                    && format.as_deref() == Some("#{window_index}:#{window_name}")
        ));

        let args = Args::parse_from([
            "rmux",
            "set-window-option",
            "--target",
            "work",
            "synchronize-panes",
            "on",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::SetWindowOption {
                target,
                option,
                value
            }) if target.as_deref() == Some("work")
                && option == "synchronize-panes"
                && value == "on"
                && matches!(
                    parse_window_option(&option, &value).unwrap(),
                    RmuxCommand::SetSynchronizePanes { enabled: true }
                )
        ));
        assert!(parse_window_option("bad-option", "on").is_err());
        assert!(parse_on_off("maybe").is_err());

        let args = Args::parse_from([
            "rmux",
            "set-option",
            "--target",
            "work",
            "default-command",
            "cat",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::SetOption {
                target,
                option,
                value
            }) if target.as_deref() == Some("work")
                && option == "default-command"
                && value == "cat"
                && matches!(
                    parse_session_option(&option, &value).unwrap(),
                    RmuxCommand::SetDefaultCommand { command: Some(command) }
                        if command == "cat"
                )
        ));
        assert!(matches!(
            parse_session_option("default-command", "default").unwrap(),
            RmuxCommand::SetDefaultCommand { command: None }
        ));
        assert!(matches!(
            parse_session_option("prefix", "C-a").unwrap(),
            RmuxCommand::SetPrefixKey { key: 1 }
        ));
        assert_eq!(parse_prefix_key("C-b").unwrap(), 0x02);
        assert!(parse_prefix_key("M-a").is_err());
        assert!(parse_session_option("bad-option", "cat").is_err());

        let args = Args::parse_from(["rmux", "kill-window", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::KillWindow { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "kill-server"]);
        assert!(matches!(args.command, Some(Command::KillServer)));

        let args = Args::parse_from(["rmux", "rename-window", "--target", "work", "editor"]);
        assert!(matches!(
            args.command,
            Some(Command::RenameWindow { target, name })
                if target.as_deref() == Some("work") && name == "editor"
        ));

        let args = Args::parse_from(["rmux", "rename-session", "--target", "work", "client-a"]);
        assert!(matches!(
            args.command,
            Some(Command::RenameSession { target, name })
                if target.as_deref() == Some("work") && name == "client-a"
        ));

        let args = Args::parse_from(["rmux", "kill-session", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::KillSession { target }) if target.as_deref() == Some("work")
        ));

        let args = Args::parse_from(["rmux", "detach-client", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::DetachClient { target }) if target.as_deref() == Some("work")
        ));
    }

    #[test]
    fn parses_hidden_smoke_commands() {
        let args = Args::parse_from(["rmux", "send-text", "--target", "work", "hello"]);
        match args.command {
            Some(Command::SendText { target, text }) => {
                assert_eq!(target, "work");
                assert_eq!(text, "hello");
            }
            other => panic!("expected send-text, got {other:?}"),
        }

        let args = Args::parse_from(["rmux", "snapshot", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::Snapshot { target }) if target == "work"
        ));

        let args = Args::parse_from(["rmux", "shutdown-server", "--target", "work"]);
        assert!(matches!(
            args.command,
            Some(Command::ShutdownServer { target }) if target == "work"
        ));

        let args = Args::parse_from([
            "rmux",
            "hold-client",
            "--target",
            "work",
            "--millis",
            "25",
            "--ready-file",
            "/tmp/rmux-ready",
        ]);
        assert!(matches!(
            args.command,
            Some(Command::HoldClient {
                target,
                millis,
                ready_file,
            }) if target == "work"
                && millis == 25
                && ready_file.as_deref() == Some("/tmp/rmux-ready")
        ));
    }

    #[test]
    fn prefix_d_maps_to_detach() {
        let key = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE);

        assert_eq!(prefix_command(key), PrefixCommand::Detach);

        let key = KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE);
        assert_eq!(prefix_command(key), PrefixCommand::ToggleLayout);

        let key = KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE);
        assert_eq!(prefix_command(key), PrefixCommand::LastWindow);

        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::NONE);
        assert_eq!(
            prefix_command(key),
            PrefixCommand::Resize(ResizeDirection::Right)
        );

        let key = KeyEvent::new(KeyCode::Char('}'), KeyModifiers::NONE);
        assert_eq!(
            prefix_command(key),
            PrefixCommand::SwapPane(SwapDirection::Next)
        );
    }
}
