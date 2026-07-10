use anyhow::{Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientRequest {
    Resize(TtySize),
    Key(WireKey),
    Paste(String),
    Mouse(WireMouse),
    Snapshot,
    Write(Vec<u8>),
    Command(RmuxCommand),
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

impl SplitAxis {
    pub fn layout_name(self) -> &'static str {
        match self {
            Self::Horizontal => "even-horizontal",
            Self::Vertical => "even-vertical",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResizeDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SwapDirection {
    Previous,
    Next,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BoundCommand {
    SplitHorizontal,
    SplitVertical,
    NewWindow,
    NextPane,
    PreviousPane,
    KillPane,
    DetachClient,
    LastWindow,
    ToggleLayout,
}

impl BoundCommand {
    pub fn command_name(self) -> &'static str {
        match self {
            Self::SplitHorizontal => "split-window --horizontal",
            Self::SplitVertical => "split-window",
            Self::NewWindow => "new-window",
            Self::NextPane => "next-pane",
            Self::PreviousPane => "previous-pane",
            Self::KillPane => "kill-pane",
            Self::DetachClient => "detach-client",
            Self::LastWindow => "last-window",
            Self::ToggleLayout => "select-layout next",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RmuxCommand {
    NewWindow {
        name: Option<String>,
        command: Option<String>,
    },
    SplitWindow {
        axis: SplitAxis,
        command: Option<String>,
    },
    SelectWindow {
        index: usize,
    },
    NextWindow,
    PreviousWindow,
    LastWindow,
    SelectPane {
        index: usize,
    },
    NextPane,
    PreviousPane,
    SelectLayout {
        axis: SplitAxis,
    },
    ResizePane {
        direction: ResizeDirection,
        amount: u16,
    },
    SwapPane {
        direction: SwapDirection,
    },
    SwapWindow {
        direction: SwapDirection,
    },
    MoveWindow {
        index: usize,
    },
    SendPrefix,
    BreakPane {
        name: Option<String>,
    },
    JoinPane {
        source_window: usize,
        source_pane: usize,
    },
    DetachClient,
    KillPane {
        all_other: bool,
    },
    RespawnPane {
        command: Option<String>,
    },
    RespawnWindow {
        command: Option<String>,
    },
    SetSynchronizePanes {
        enabled: bool,
    },
    SetDefaultCommand {
        command: Option<String>,
    },
    SetPrefixKey {
        key: u8,
    },
    BindKey {
        key: char,
        command: BoundCommand,
    },
    KillWindow,
    RenameWindow {
        name: String,
    },
    RenameSession {
        name: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerResponse {
    View(SessionView),
    Noop,
    Detached,
    Shutdown,
    Error(String),
}

impl ServerResponse {
    pub fn into_view(self) -> Result<SessionView> {
        match self {
            Self::View(view) => Ok(view),
            Self::Noop => Err(anyhow!("request did not produce a view")),
            Self::Detached => Err(anyhow!("session detached")),
            Self::Shutdown => Err(anyhow!("session shut down")),
            Self::Error(message) => Err(anyhow!(message)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_response_becomes_error() {
        let error = ServerResponse::Error("bad target".to_string())
            .into_view()
            .expect_err("error response should not produce a view");

        assert_eq!(error.to_string(), "bad target");
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionView {
    pub session: String,
    #[serde(default)]
    pub default_command: Option<String>,
    #[serde(default = "default_prefix_key")]
    pub prefix_key: String,
    #[serde(default)]
    pub key_bindings: Vec<KeyBindingView>,
    pub windows: Vec<WindowTabView>,
    pub active_window: usize,
    pub panes: Vec<PaneView>,
    pub active_pane: usize,
    pub split_axis: SplitAxis,
    pub pane_weights: Vec<u16>,
    pub prefix: bool,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyBindingView {
    pub key: char,
    pub command: BoundCommand,
}

fn default_prefix_key() -> String {
    "C-b".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowTabView {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub synchronize_panes: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneView {
    pub id: u64,
    #[serde(default)]
    pub x: u16,
    #[serde(default)]
    pub y: u16,
    #[serde(default)]
    pub width: u16,
    #[serde(default)]
    pub height: u16,
    #[serde(default)]
    pub cols: u16,
    #[serde(default)]
    pub rows: u16,
    #[serde(default)]
    pub cells: Vec<PaneCell>,
    #[serde(default)]
    pub cursor: Option<CursorView>,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneCell {
    pub col: usize,
    pub row: usize,
    pub ch: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub bold: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorView {
    pub col: usize,
    pub row: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireKey {
    code: WireKeyCode,
    modifiers: u8,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct WireMouse {
    pub col: u16,
    pub row: u16,
    pub kind: WireMouseKind,
    pub modifiers: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireMouseKind {
    Down(WireMouseButton),
    Up(WireMouseButton),
    Drag(WireMouseButton),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WireMouseButton {
    Left,
    Middle,
    Right,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
enum WireKeyCode {
    Char(char),
    Enter,
    Backspace,
    Tab,
    Esc,
    Up,
    Down,
    Right,
    Left,
    Home,
    End,
    Delete,
    Unsupported,
}

impl From<KeyEvent> for WireKey {
    fn from(key: KeyEvent) -> Self {
        let mut modifiers = 0;
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            modifiers |= 1;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            modifiers |= 2;
        }
        if key.modifiers.contains(KeyModifiers::ALT) {
            modifiers |= 4;
        }
        if key.modifiers.contains(KeyModifiers::SUPER) {
            modifiers |= 8;
        }

        Self {
            code: match key.code {
                KeyCode::Char(ch) => WireKeyCode::Char(ch),
                KeyCode::Enter => WireKeyCode::Enter,
                KeyCode::Backspace => WireKeyCode::Backspace,
                KeyCode::Tab => WireKeyCode::Tab,
                KeyCode::Esc => WireKeyCode::Esc,
                KeyCode::Up => WireKeyCode::Up,
                KeyCode::Down => WireKeyCode::Down,
                KeyCode::Right => WireKeyCode::Right,
                KeyCode::Left => WireKeyCode::Left,
                KeyCode::Home => WireKeyCode::Home,
                KeyCode::End => WireKeyCode::End,
                KeyCode::Delete => WireKeyCode::Delete,
                _ => WireKeyCode::Unsupported,
            },
            modifiers,
        }
    }
}

impl From<WireKey> for KeyEvent {
    fn from(key: WireKey) -> Self {
        let mut modifiers = KeyModifiers::empty();
        if key.modifiers & 1 != 0 {
            modifiers |= KeyModifiers::SHIFT;
        }
        if key.modifiers & 2 != 0 {
            modifiers |= KeyModifiers::CONTROL;
        }
        if key.modifiers & 4 != 0 {
            modifiers |= KeyModifiers::ALT;
        }
        if key.modifiers & 8 != 0 {
            modifiers |= KeyModifiers::SUPER;
        }

        let code = match key.code {
            WireKeyCode::Char(ch) => KeyCode::Char(ch),
            WireKeyCode::Enter => KeyCode::Enter,
            WireKeyCode::Backspace => KeyCode::Backspace,
            WireKeyCode::Tab => KeyCode::Tab,
            WireKeyCode::Esc => KeyCode::Esc,
            WireKeyCode::Up => KeyCode::Up,
            WireKeyCode::Down => KeyCode::Down,
            WireKeyCode::Right => KeyCode::Right,
            WireKeyCode::Left => KeyCode::Left,
            WireKeyCode::Home => KeyCode::Home,
            WireKeyCode::End => KeyCode::End,
            WireKeyCode::Delete => KeyCode::Delete,
            WireKeyCode::Unsupported => KeyCode::Null,
        };
        KeyEvent::new(code, modifiers)
    }
}

impl From<MouseEvent> for WireMouse {
    fn from(mouse: MouseEvent) -> Self {
        let mut modifiers = 0;
        if mouse.modifiers.contains(KeyModifiers::SHIFT) {
            modifiers |= 1;
        }
        if mouse.modifiers.contains(KeyModifiers::CONTROL) {
            modifiers |= 2;
        }
        if mouse.modifiers.contains(KeyModifiers::ALT) {
            modifiers |= 4;
        }
        if mouse.modifiers.contains(KeyModifiers::SUPER) {
            modifiers |= 8;
        }

        Self {
            col: mouse.column,
            row: mouse.row,
            kind: match mouse.kind {
                MouseEventKind::Down(button) => WireMouseKind::Down(WireMouseButton::from(button)),
                MouseEventKind::Up(button) => WireMouseKind::Up(WireMouseButton::from(button)),
                MouseEventKind::Drag(button) => WireMouseKind::Drag(WireMouseButton::from(button)),
                MouseEventKind::Moved => WireMouseKind::Moved,
                MouseEventKind::ScrollUp => WireMouseKind::ScrollUp,
                MouseEventKind::ScrollDown => WireMouseKind::ScrollDown,
                MouseEventKind::ScrollLeft => WireMouseKind::ScrollLeft,
                MouseEventKind::ScrollRight => WireMouseKind::ScrollRight,
            },
            modifiers,
        }
    }
}

impl From<MouseButton> for WireMouseButton {
    fn from(button: MouseButton) -> Self {
        match button {
            MouseButton::Left => Self::Left,
            MouseButton::Right => Self::Right,
            MouseButton::Middle => Self::Middle,
        }
    }
}
