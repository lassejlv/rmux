use std::{cmp::max, collections::VecDeque};

use anyhow::{Context, Result, anyhow};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use termy_core::{
    Terminal, TerminalEvent, TerminalMouseButton, TerminalMouseEventKind, TerminalMouseModifiers,
    TerminalMousePosition, TerminalReplyHost, TerminalRuntimeConfig, TerminalSize, TermyFrame,
    encode_mouse_report, load_config_from_default_path, measure_cell_from_config,
};

use crate::{
    STATUS_ROWS,
    protocol::{
        BoundCommand, KeyBindingView, PaneView, ResizeDirection, SessionView, SplitAxis,
        SwapDirection, TtySize, WindowTabView, WireMouse, WireMouseButton, WireMouseKind,
    },
    render::frame_cells_and_text_lines,
};

pub struct RawPane {
    id: u64,
    rect: Rect,
    cols: u16,
    rows: u16,
    max_width: usize,
    frame: TermyFrame,
}

pub struct RawView {
    panes: Vec<RawPane>,
    session: String,
    default_command: Option<String>,
    prefix_key: String,
    key_bindings: Vec<KeyBindingView>,
    windows: Vec<WindowTabView>,
    active_window: usize,
    active_pane: usize,
    split_axis: SplitAxis,
    pane_weights: Vec<u16>,
    prefix: bool,
    message: Option<String>,
}

pub fn build_session_view(raw: RawView) -> SessionView {
    let mut panes = Vec::with_capacity(raw.panes.len());
    for rp in raw.panes {
        let (cells, lines) = frame_cells_and_text_lines(&rp.frame, rp.max_width);
        let cursor = rp.frame.cursor.map(|cursor| crate::protocol::CursorView {
            col: cursor.col,
            row: cursor.row,
        });
        panes.push(PaneView {
            id: rp.id,
            x: rp.rect.x,
            y: rp.rect.y,
            width: rp.rect.width,
            height: rp.rect.height,
            cols: rp.cols,
            rows: rp.rows,
            cells,
            cursor,
            lines,
        });
    }

    SessionView {
        session: raw.session,
        default_command: raw.default_command,
        prefix_key: raw.prefix_key,
        key_bindings: raw.key_bindings,
        windows: raw.windows,
        active_window: raw.active_window,
        panes,
        active_pane: raw.active_pane,
        split_axis: raw.split_axis,
        pane_weights: raw.pane_weights,
        prefix: raw.prefix,
        message: raw.message,
    }
}

const DEFAULT_CELL_WIDTH: f32 = 9.0;
const DEFAULT_CELL_HEIGHT: f32 = 18.0;
const MIN_PANE_COLS: u16 = 10;
const MIN_PANE_ROWS: u16 = 3;
const EVEN_PANE_WEIGHT: u16 = 100;
const DEFAULT_PREFIX_KEY: u8 = 0x02;
const MAX_PANES_PER_WINDOW: usize = 64;
const MAX_WINDOWS_PER_SESSION: usize = 64;
const BRACKETED_PASTE_START: &[u8] = b"\x1b[200~";
const BRACKETED_PASTE_END: &[u8] = b"\x1b[201~";

fn sanitize_bracketed_paste_input(input: &[u8]) -> Option<Vec<u8>> {
    let mut sanitized = None;
    let mut index = 0;
    while index < input.len() {
        let remaining = &input[index..];
        let marker_len = if remaining.starts_with(BRACKETED_PASTE_END) {
            Some(BRACKETED_PASTE_END.len())
        } else if remaining.starts_with(BRACKETED_PASTE_START) {
            Some(BRACKETED_PASTE_START.len())
        } else {
            None
        };

        if let Some(marker_len) = marker_len {
            if sanitized.is_none() {
                let mut buffer = Vec::with_capacity(input.len());
                buffer.extend_from_slice(&input[..index]);
                sanitized = Some(buffer);
            }
            index += marker_len;
            continue;
        }

        if let Some(buffer) = sanitized.as_mut() {
            buffer.push(input[index]);
        }
        index += 1;
    }
    sanitized
}

fn framed_bracketed_paste_input(input: &[u8]) -> Vec<u8> {
    let sanitized = sanitize_bracketed_paste_input(input);
    let payload = sanitized.as_deref().unwrap_or(input);
    let mut framed =
        Vec::with_capacity(BRACKETED_PASTE_START.len() + payload.len() + BRACKETED_PASTE_END.len());
    framed.extend_from_slice(BRACKETED_PASTE_START);
    framed.extend_from_slice(payload);
    framed.extend_from_slice(BRACKETED_PASTE_END);
    framed
}

#[derive(Clone, Debug)]
pub struct TerminalConfig {
    cell_width: f32,
    cell_height: f32,
    working_dir: Option<String>,
    runtime_config: TerminalRuntimeConfig,
}

impl TerminalConfig {
    pub fn load() -> Self {
        load_config_from_default_path()
            .ok()
            .map(|loaded| {
                let metrics = measure_cell_from_config(&loaded.app_config);
                Self {
                    cell_width: metrics.cell_width,
                    cell_height: metrics.cell_height,
                    working_dir: loaded.app_config.working_dir,
                    runtime_config: loaded.runtime_config,
                }
            })
            .unwrap_or(Self {
                cell_width: DEFAULT_CELL_WIDTH,
                cell_height: DEFAULT_CELL_HEIGHT,
                working_dir: None,
                runtime_config: TerminalRuntimeConfig::default(),
            })
    }

    fn size(&self, cols: u16, rows: u16) -> TerminalSize {
        TerminalSize {
            cols,
            rows,
            cell_width: self.cell_width,
            cell_height: self.cell_height,
        }
    }

    fn runtime_config(&self) -> &TerminalRuntimeConfig {
        &self.runtime_config
    }

    fn working_dir(&self) -> Option<&str> {
        self.working_dir.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn for_test(working_dir: Option<String>) -> Self {
        Self {
            cell_width: DEFAULT_CELL_WIDTH,
            cell_height: DEFAULT_CELL_HEIGHT,
            working_dir,
            runtime_config: TerminalRuntimeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PaneId(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WindowId(u64);

struct Pane {
    id: PaneId,
    terminal: Terminal,
    last_rect: Option<Rect>,
}

impl Pane {
    fn new(
        id: PaneId,
        size: TerminalSize,
        config: &TerminalConfig,
        command: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            id,
            terminal: Terminal::new(
                size,
                config.working_dir(),
                None,
                None,
                Some(config.runtime_config()),
                command,
            )
            .context("spawn libtermy terminal")?,
            last_rect: None,
        })
    }

    fn resize(&mut self, rect: Rect, config: &TerminalConfig, framed: bool) {
        let inset = u16::from(framed) * 2;
        let cols = max(MIN_PANE_COLS, rect.width.saturating_sub(inset));
        let rows = max(MIN_PANE_ROWS, rect.height.saturating_sub(inset));
        let size = config.size(cols, rows);
        if self.last_rect != Some(rect)
            || self.terminal.size().cols != cols
            || self.terminal.size().rows != rows
        {
            self.terminal.resize(size);
            self.last_rect = Some(rect);
        }
    }

    fn write(&self, bytes: &[u8]) {
        self.terminal.write(bytes);
    }

    fn paste(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        if self.terminal.bracketed_paste_mode() {
            self.write(&framed_bracketed_paste_input(bytes));
        } else {
            self.write(bytes);
        }
    }

    fn respawn(&mut self, config: &TerminalConfig, command: Option<&str>) -> Result<()> {
        let size = self.terminal.size();
        self.terminal = Terminal::new(
            size,
            config.working_dir(),
            None,
            None,
            Some(config.runtime_config()),
            command,
        )
        .context("respawn libtermy terminal")?;
        self.last_rect = None;
        Ok(())
    }
}

struct Window {
    id: WindowId,
    name: String,
    panes: Vec<Pane>,
    pane_weights: Vec<u16>,
    layout: PaneLayout,
    active: usize,
    split_axis: SplitAxis,
    synchronize_panes: bool,
}

#[derive(Clone, Debug)]
enum PaneLayout {
    Leaf(usize),
    Split {
        axis: SplitAxis,
        first_weight: u16,
        second_weight: u16,
        first: Box<PaneLayout>,
        second: Box<PaneLayout>,
    },
}

impl PaneLayout {
    fn single() -> Self {
        Self::Leaf(0)
    }

    fn even(axis: SplitAxis, count: usize) -> Self {
        Self::even_from(axis, 0, count)
    }

    fn even_from(axis: SplitAxis, start: usize, count: usize) -> Self {
        if count <= 1 {
            return Self::Leaf(start);
        }
        Self::Split {
            axis,
            first_weight: EVEN_PANE_WEIGHT,
            second_weight: EVEN_PANE_WEIGHT.saturating_mul(count.saturating_sub(1) as u16),
            first: Box::new(Self::Leaf(start)),
            second: Box::new(Self::even_from(axis, start + 1, count - 1)),
        }
    }

    fn rects(&self, area: Rect, out: &mut [Rect]) {
        match self {
            Self::Leaf(index) => {
                if let Some(rect) = out.get_mut(*index) {
                    *rect = area;
                }
            }
            Self::Split {
                axis,
                first_weight,
                second_weight,
                first,
                second,
            } => {
                let (first_rect, second_rect) =
                    split_rect(area, *axis, *first_weight, *second_weight);
                first.rects(first_rect, out);
                second.rects(second_rect, out);
            }
        }
    }

    fn split_leaf(&mut self, target: usize, axis: SplitAxis, new_index: usize) -> bool {
        match self {
            Self::Leaf(index) if *index == target => {
                *self = Self::Split {
                    axis,
                    first_weight: EVEN_PANE_WEIGHT,
                    second_weight: EVEN_PANE_WEIGHT,
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new_index)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                first.split_leaf(target, axis, new_index)
                    || second.split_leaf(target, axis, new_index)
            }
        }
    }

    fn remove_leaf(&mut self, target: usize) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split { first, second, .. } => {
                if matches!(first.as_ref(), Self::Leaf(index) if *index == target) {
                    let replacement = std::mem::replace(second.as_mut(), Self::single());
                    *self = replacement;
                    return true;
                }
                if matches!(second.as_ref(), Self::Leaf(index) if *index == target) {
                    let replacement = std::mem::replace(first.as_mut(), Self::single());
                    *self = replacement;
                    return true;
                }
                first.remove_leaf(target) || second.remove_leaf(target)
            }
        }
    }

    fn leaf_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Split { first, second, .. } => first.leaf_count() + second.leaf_count(),
        }
    }

    fn remap_indices(&mut self, remap: impl Fn(usize) -> usize + Copy) {
        match self {
            Self::Leaf(index) => *index = remap(*index),
            Self::Split { first, second, .. } => {
                first.remap_indices(remap);
                second.remap_indices(remap);
            }
        }
    }

    fn contains_leaf(&self, target: usize) -> bool {
        match self {
            Self::Leaf(index) => *index == target,
            Self::Split { first, second, .. } => {
                first.contains_leaf(target) || second.contains_leaf(target)
            }
        }
    }

    fn resize_leaf(&mut self, target: usize, direction: ResizeDirection, amount: u16) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Split {
                axis,
                first_weight,
                second_weight,
                first,
                second,
            } => {
                if first.resize_leaf(target, direction, amount)
                    || second.resize_leaf(target, direction, amount)
                {
                    return true;
                }

                let target_in_first = first.contains_leaf(target);
                let target_in_second = second.contains_leaf(target);
                let grow_first = matches!(
                    (*axis, direction, target_in_first, target_in_second),
                    (SplitAxis::Horizontal, ResizeDirection::Right, true, false)
                        | (SplitAxis::Vertical, ResizeDirection::Down, true, false)
                );
                let grow_second = matches!(
                    (*axis, direction, target_in_first, target_in_second),
                    (SplitAxis::Horizontal, ResizeDirection::Left, false, true)
                        | (SplitAxis::Vertical, ResizeDirection::Up, false, true)
                );
                if grow_first {
                    return apply_weight_delta(first_weight, second_weight, amount);
                }
                if grow_second {
                    return apply_weight_delta(second_weight, first_weight, amount);
                }
                false
            }
        }
    }

    fn root_axis(&self) -> SplitAxis {
        match self {
            Self::Leaf(_) => SplitAxis::Horizontal,
            Self::Split { axis, .. } => *axis,
        }
    }

    fn pane_weights(&self, pane_count: usize) -> Vec<u16> {
        if pane_count == 2
            && let Self::Split {
                first_weight,
                second_weight,
                first,
                second,
                ..
            } = self
            && matches!((&**first, &**second), (Self::Leaf(0), Self::Leaf(1)))
        {
            return vec![*first_weight, *second_weight];
        }
        vec![EVEN_PANE_WEIGHT; pane_count]
    }
}

impl Window {
    fn new(
        id: WindowId,
        pane_id: PaneId,
        size: TerminalSize,
        config: &TerminalConfig,
        command: Option<&str>,
    ) -> Result<Self> {
        Ok(Self {
            id,
            name: format!("win{}", id.0),
            panes: vec![Pane::new(pane_id, size, config, command)?],
            pane_weights: vec![EVEN_PANE_WEIGHT],
            layout: PaneLayout::single(),
            active: 0,
            split_axis: SplitAxis::Horizontal,
            synchronize_panes: false,
        })
    }

    fn from_pane(id: WindowId, name: String, pane: Pane, weight: u16) -> Self {
        Self {
            id,
            name,
            panes: vec![pane],
            pane_weights: vec![weight],
            layout: PaneLayout::single(),
            active: 0,
            split_axis: SplitAxis::Horizontal,
            synchronize_panes: false,
        }
    }

    fn active_pane(&self) -> &Pane {
        let active = self.active.min(self.panes.len().saturating_sub(1));
        &self.panes[active]
    }

    fn write_input(&self, bytes: &[u8]) {
        if self.synchronize_panes {
            for pane in &self.panes {
                pane.write(bytes);
            }
        } else {
            self.active_pane().write(bytes);
        }
    }

    fn paste_input(&self, bytes: &[u8]) {
        if self.synchronize_panes {
            for pane in &self.panes {
                pane.paste(bytes);
            }
        } else {
            self.active_pane().paste(bytes);
        }
    }

    fn split(
        &mut self,
        axis: SplitAxis,
        pane_id: PaneId,
        size: TerminalSize,
        config: &TerminalConfig,
        command: Option<&str>,
    ) -> Result<()> {
        if self.panes.is_empty() {
            return Err(anyhow!("no pane to split"));
        }
        if self.panes.len() >= MAX_PANES_PER_WINDOW {
            return Err(anyhow!("pane limit reached (max {MAX_PANES_PER_WINDOW})"));
        }
        let active = self.active.min(self.panes.len() - 1);
        let insert_at = active + 1;
        self.layout
            .remap_indices(|index| if index >= insert_at { index + 1 } else { index });
        let split_applied = self.layout.split_leaf(active, axis, insert_at);
        self.panes
            .insert(insert_at, Pane::new(pane_id, size, config, command)?);
        self.pane_weights.insert(insert_at, EVEN_PANE_WEIGHT);
        self.active = insert_at;
        if !split_applied {
            self.layout = PaneLayout::even(axis, self.panes.len());
        }
        self.repair_layout();
        self.sync_legacy_layout_fields();
        Ok(())
    }

    fn kill_active_pane(&mut self) -> bool {
        if self.panes.len() <= 1 || self.active >= self.panes.len() {
            return false;
        }
        self.take_pane(self.active).is_some()
    }

    fn take_active_pane(&mut self) -> Option<(Pane, u16)> {
        if self.panes.len() <= 1 {
            return None;
        }
        self.take_pane(self.active)
    }

    fn take_pane(&mut self, index: usize) -> Option<(Pane, u16)> {
        if index >= self.panes.len() {
            return None;
        }
        let pane = self.panes.remove(index);
        let weight = self.pane_weights.remove(index);
        self.layout.remove_leaf(index);
        self.layout.remap_indices(|pane_index| {
            if pane_index > index {
                pane_index - 1
            } else {
                pane_index
            }
        });
        if self.active > index {
            self.active -= 1;
        }
        self.active = self.active.min(self.panes.len().saturating_sub(1));
        self.repair_layout();
        self.sync_legacy_layout_fields();
        Some((pane, weight))
    }

    fn kill_pane(&mut self, index: usize) -> bool {
        if self.panes.len() <= 1 {
            return false;
        }
        self.take_pane(index).is_some()
    }

    fn push_pane(&mut self, pane: Pane, weight: u16) {
        let new_index = self.panes.len();
        self.panes.push(pane);
        self.pane_weights.push(weight);
        if !self
            .layout
            .split_leaf(self.active, SplitAxis::Horizontal, new_index)
        {
            self.layout = PaneLayout::even(SplitAxis::Horizontal, self.panes.len());
        }
        self.active = self.panes.len() - 1;
        self.repair_layout();
        self.sync_legacy_layout_fields();
    }

    fn kill_other_panes(&mut self) -> bool {
        if self.panes.len() <= 1 || self.active >= self.panes.len() {
            return false;
        }
        let pane = self.panes.remove(self.active);
        let weight = self.pane_weights.remove(self.active);
        self.panes.clear();
        self.pane_weights.clear();
        self.panes.push(pane);
        self.pane_weights.push(weight);
        self.layout = PaneLayout::single();
        self.active = 0;
        self.sync_legacy_layout_fields();
        true
    }

    fn respawn_active_pane(
        &mut self,
        config: &TerminalConfig,
        command: Option<&str>,
    ) -> Result<()> {
        let active = self.active.min(self.panes.len().saturating_sub(1));
        let Some(pane) = self.panes.get_mut(active) else {
            return Err(anyhow!("no active pane to respawn"));
        };
        pane.respawn(config, command)
    }

    fn respawn(
        &mut self,
        pane_id: PaneId,
        size: TerminalSize,
        config: &TerminalConfig,
        command: Option<&str>,
    ) -> Result<()> {
        self.panes.clear();
        self.pane_weights.clear();
        self.panes.push(Pane::new(pane_id, size, config, command)?);
        self.pane_weights.push(EVEN_PANE_WEIGHT);
        self.layout = PaneLayout::single();
        self.active = 0;
        self.sync_legacy_layout_fields();
        self.synchronize_panes = false;
        Ok(())
    }

    fn next_pane(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        self.active = (self.active + 1) % self.panes.len();
    }

    fn previous_pane(&mut self) {
        if self.panes.is_empty() {
            return;
        }
        self.active = if self.active == 0 {
            self.panes.len() - 1
        } else {
            self.active - 1
        };
    }

    fn select_pane(&mut self, index: usize) -> bool {
        if index < self.panes.len() {
            self.active = index;
            true
        } else {
            false
        }
    }

    fn swap_active_pane(&mut self, direction: SwapDirection) -> bool {
        if self.active >= self.panes.len() {
            return false;
        }
        let Some(other) = self.swap_index(direction) else {
            return false;
        };
        self.panes.swap(self.active, other);
        self.pane_weights.swap(self.active, other);
        self.active = other;
        true
    }

    fn swap_index(&self, direction: SwapDirection) -> Option<usize> {
        match direction {
            SwapDirection::Previous if self.active > 0 => Some(self.active - 1),
            SwapDirection::Next if self.active + 1 < self.panes.len() => Some(self.active + 1),
            _ => None,
        }
    }

    fn resize_active_pane(&mut self, direction: ResizeDirection, amount: u16) -> bool {
        if self.panes.len() <= 1 || amount == 0 {
            return false;
        }
        let resized = self.layout.resize_leaf(self.active, direction, amount);
        self.sync_legacy_layout_fields();
        resized
    }

    fn select_layout(&mut self, axis: SplitAxis) {
        self.layout = PaneLayout::even(axis, self.panes.len());
        self.sync_legacy_layout_fields();
    }

    fn layout_rects(&self, area: Rect) -> Vec<Rect> {
        let mut rects = vec![area; self.panes.len()];
        self.layout.rects(area, &mut rects);
        rects
    }

    fn sync_legacy_layout_fields(&mut self) {
        self.split_axis = self.layout.root_axis();
        self.pane_weights = self.layout.pane_weights(self.panes.len());
    }

    /// Rebuild an even layout if the tree no longer maps one leaf to each pane.
    fn repair_layout(&mut self) {
        let count = self.panes.len();
        let consistent = self.layout.leaf_count() == count
            && (0..count).all(|index| self.layout.contains_leaf(index));
        if !consistent {
            self.layout = PaneLayout::even(self.layout.root_axis(), count);
        }
    }
}

struct Session {
    name: String,
    default_command: Option<String>,
    prefix_key: u8,
    key_bindings: Vec<(char, BoundCommand)>,
    windows: Vec<Window>,
    active_window: usize,
    last_window: Option<usize>,
}

impl Session {
    fn new(name: impl Into<String>, first_window: Window) -> Self {
        Self {
            name: name.into(),
            default_command: None,
            prefix_key: DEFAULT_PREFIX_KEY,
            key_bindings: Vec::new(),
            windows: vec![first_window],
            active_window: 0,
            last_window: None,
        }
    }

    fn active_window(&self) -> &Window {
        let active = self.active_window.min(self.windows.len().saturating_sub(1));
        &self.windows[active]
    }

    fn active_window_mut(&mut self) -> &mut Window {
        let active = self.active_window.min(self.windows.len().saturating_sub(1));
        &mut self.windows[active]
    }

    fn push_window(&mut self, window: Window) {
        self.windows.push(window);
        self.set_active_window(self.windows.len() - 1);
    }

    fn select_window(&mut self, index: usize) -> bool {
        if index < self.windows.len() {
            self.set_active_window(index);
            true
        } else {
            false
        }
    }

    fn next_window(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        self.set_active_window((self.active_window + 1) % self.windows.len());
    }

    fn previous_window(&mut self) {
        if self.windows.is_empty() {
            return;
        }
        let next = if self.active_window == 0 {
            self.windows.len() - 1
        } else {
            self.active_window - 1
        };
        self.set_active_window(next);
    }

    fn last_window(&mut self) -> bool {
        let Some(last) = self.last_window else {
            return false;
        };
        if last >= self.windows.len() || last == self.active_window {
            return false;
        }
        self.set_active_window(last);
        true
    }

    fn kill_active_window(&mut self) -> bool {
        if self.windows.len() <= 1 || self.active_window >= self.windows.len() {
            return false;
        }
        let removed = self.active_window;
        self.windows.remove(self.active_window);
        self.active_window = self.active_window.min(self.windows.len().saturating_sub(1));
        self.last_window = self
            .last_window
            .and_then(|last| normalize_removed_index(last, removed));
        true
    }

    fn kill_window(&mut self, index: usize) -> bool {
        if self.windows.len() <= 1 || index >= self.windows.len() {
            return false;
        }
        self.windows.remove(index);
        self.active_window = if self.active_window > index {
            self.active_window - 1
        } else {
            self.active_window.min(self.windows.len().saturating_sub(1))
        };
        self.last_window = self
            .last_window
            .and_then(|last| normalize_removed_index(last, index));
        true
    }

    fn swap_active_window(&mut self, direction: SwapDirection) -> bool {
        let Some(other) = self.swap_index(direction) else {
            return false;
        };
        self.windows.swap(self.active_window, other);
        self.remap_last_after_swap(self.active_window, other);
        self.set_active_window(other);
        true
    }

    fn move_active_window(&mut self, index: usize) -> bool {
        if index >= self.windows.len() || self.active_window >= self.windows.len() {
            return false;
        }
        if index == self.active_window {
            return true;
        }
        let from = self.active_window;
        let window = self.windows.remove(self.active_window);
        self.windows.insert(index, window);
        self.last_window = self
            .last_window
            .map(|last| remap_moved_index(last, from, index));
        self.set_active_window(index);
        true
    }

    fn join_pane_into_active(&mut self, source_window: usize, source_pane: usize) -> bool {
        if source_window >= self.windows.len() || source_window == self.active_window {
            return false;
        }

        let destination = self.active_window;
        let (pane, weight) = if self.windows[source_window].panes.len() == 1 {
            if source_pane != 0 {
                return false;
            }
            let mut source = self.windows.remove(source_window);
            self.last_window = self
                .last_window
                .and_then(|last| normalize_removed_index(last, source_window));
            self.active_window = if source_window < destination {
                destination - 1
            } else {
                destination
            };
            let Some((pane, weight)) = source.take_pane(0) else {
                return false;
            };
            (pane, weight)
        } else {
            let Some((pane, weight)) = self.windows[source_window].take_pane(source_pane) else {
                return false;
            };
            (pane, weight)
        };

        self.active_window_mut().push_pane(pane, weight);
        true
    }

    fn swap_index(&self, direction: SwapDirection) -> Option<usize> {
        match direction {
            SwapDirection::Previous if self.active_window > 0 => Some(self.active_window - 1),
            SwapDirection::Next if self.active_window + 1 < self.windows.len() => {
                Some(self.active_window + 1)
            }
            _ => None,
        }
    }

    fn set_active_window(&mut self, index: usize) {
        if index != self.active_window {
            self.last_window = Some(self.active_window);
            self.active_window = index;
        }
    }

    fn remap_last_after_swap(&mut self, a: usize, b: usize) {
        self.last_window = self.last_window.map(|last| {
            if last == a {
                b
            } else if last == b {
                a
            } else {
                last
            }
        });
    }
}

fn normalize_removed_index(index: usize, removed: usize) -> Option<usize> {
    match index.cmp(&removed) {
        std::cmp::Ordering::Less => Some(index),
        std::cmp::Ordering::Equal => None,
        std::cmp::Ordering::Greater => Some(index - 1),
    }
}

fn remap_moved_index(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < to && index > from && index <= to {
        index - 1
    } else if to < from && index >= to && index < from {
        index + 1
    } else {
        index
    }
}

fn split_rect(area: Rect, axis: SplitAxis, first_weight: u16, second_weight: u16) -> (Rect, Rect) {
    let total = u32::from(first_weight) + u32::from(second_weight);
    if total == 0 {
        return (area, area);
    }
    match axis {
        SplitAxis::Horizontal => {
            let first_width = ((u32::from(area.width) * u32::from(first_weight)) / total) as u16;
            let first_width = first_width.max(1).min(area.width);
            let second_width = area.width.saturating_sub(first_width);
            (
                Rect {
                    width: first_width,
                    ..area
                },
                Rect {
                    x: area.x.saturating_add(first_width),
                    width: second_width,
                    ..area
                },
            )
        }
        SplitAxis::Vertical => {
            let first_height = ((u32::from(area.height) * u32::from(first_weight)) / total) as u16;
            let first_height = first_height.max(1).min(area.height);
            let second_height = area.height.saturating_sub(first_height);
            (
                Rect {
                    height: first_height,
                    ..area
                },
                Rect {
                    y: area.y.saturating_add(first_height),
                    height: second_height,
                    ..area
                },
            )
        }
    }
}

fn apply_weight_delta(grow: &mut u16, shrink: &mut u16, amount: u16) -> bool {
    let applied = amount.min(shrink.saturating_sub(1));
    if applied == 0 {
        return false;
    }
    *grow = grow.saturating_add(applied);
    *shrink = shrink.saturating_sub(applied);
    true
}

fn terminal_mouse_event(kind: WireMouseKind) -> Option<TerminalMouseEventKind> {
    match kind {
        WireMouseKind::Down(button) => Some(TerminalMouseEventKind::Press(terminal_button(button))),
        WireMouseKind::Up(button) => Some(TerminalMouseEventKind::Release(terminal_button(button))),
        WireMouseKind::Drag(button) => Some(TerminalMouseEventKind::Drag(terminal_button(button))),
        WireMouseKind::Moved => Some(TerminalMouseEventKind::Move),
        WireMouseKind::ScrollUp => Some(TerminalMouseEventKind::WheelUp),
        WireMouseKind::ScrollDown => Some(TerminalMouseEventKind::WheelDown),
        WireMouseKind::ScrollLeft => Some(TerminalMouseEventKind::WheelLeft),
        WireMouseKind::ScrollRight => Some(TerminalMouseEventKind::WheelRight),
    }
}

fn terminal_button(button: WireMouseButton) -> TerminalMouseButton {
    match button {
        WireMouseButton::Left => TerminalMouseButton::Left,
        WireMouseButton::Middle => TerminalMouseButton::Middle,
        WireMouseButton::Right => TerminalMouseButton::Right,
    }
}

fn terminal_mouse_modifiers(modifiers: u8) -> TerminalMouseModifiers {
    TerminalMouseModifiers {
        shift: modifiers & 1 != 0,
        control: modifiers & 2 != 0,
        alt: modifiers & 4 != 0,
    }
}

pub struct Rmux {
    session: Session,
    next_pane_id: u64,
    next_window_id: u64,
    prefix: bool,
    messages: VecDeque<String>,
    tty_size: TtySize,
    terminal_config: TerminalConfig,
    exit_reason: Option<ExitReason>,
    detach_generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Quit,
    Detach,
}

impl Rmux {
    pub fn new(
        tty_size: TtySize,
        terminal_config: TerminalConfig,
        session_name: impl Into<String>,
        command: Option<&str>,
    ) -> Result<Self> {
        let pane_cols = max(MIN_PANE_COLS, tty_size.cols.saturating_sub(2));
        let pane_rows = max(MIN_PANE_ROWS, tty_size.rows.saturating_sub(STATUS_ROWS + 2));
        let first_window = Window::new(
            WindowId(1),
            PaneId(1),
            terminal_config.size(pane_cols, pane_rows),
            &terminal_config,
            command,
        )?;
        let mut app = Self {
            session: Session::new(session_name, first_window),
            next_pane_id: 2,
            next_window_id: 2,
            prefix: false,
            messages: VecDeque::new(),
            tty_size,
            terminal_config,
            exit_reason: None,
            detach_generation: 0,
        };
        app.message("split: ^D/^Shift-D  click pane  detach: d  quit: q");
        Ok(app)
    }

    pub fn resize(&mut self, tty_size: TtySize) {
        self.tty_size = tty_size;
    }

    #[cfg(test)]
    pub fn session_view(&mut self) -> SessionView {
        build_session_view(self.collect_raw_view())
    }

    pub fn collect_raw_view(&mut self) -> RawView {
        let pane_area = Rect {
            x: 0,
            y: 0,
            width: self.tty_size.cols,
            height: self.tty_size.rows.saturating_sub(STATUS_ROWS),
        };
        let layouts = self.active_window().layout_rects(pane_area);
        let terminal_config = self.terminal_config.clone();
        let framed = layouts.len() > 1;
        let mut panes = Vec::with_capacity(layouts.len());
        for (index, rect) in layouts.into_iter().enumerate() {
            let window = self.active_window_mut();
            let Some(pane) = window.panes.get_mut(index) else {
                continue;
            };
            pane.resize(rect, &terminal_config, framed);
            let size = pane.terminal.size();
            let frame = pane.terminal.snapshot();
            let max_width = if framed {
                rect.width.saturating_sub(2)
            } else {
                rect.width
            } as usize;
            panes.push(RawPane {
                id: pane.id.0,
                rect,
                cols: size.cols,
                rows: size.rows,
                max_width,
                frame,
            });
        }

        RawView {
            panes,
            session: self.session.name.clone(),
            default_command: self.session.default_command.clone(),
            prefix_key: format_key_byte(self.session.prefix_key),
            key_bindings: self
                .session
                .key_bindings
                .iter()
                .map(|(key, command)| KeyBindingView {
                    key: *key,
                    command: *command,
                })
                .collect(),
            windows: self
                .session
                .windows
                .iter()
                .map(|window| WindowTabView {
                    id: window.id.0,
                    name: window.name.clone(),
                    synchronize_panes: window.synchronize_panes,
                })
                .collect(),
            active_window: self.session.active_window,
            active_pane: self.active_window().active,
            split_axis: self.active_window().split_axis,
            pane_weights: self.active_window().pane_weights.clone(),
            prefix: self.prefix,
            message: self.messages.back().cloned(),
        }
    }

    pub fn take_exit_reason(&mut self) -> Option<ExitReason> {
        self.exit_reason.take()
    }

    pub fn detach_generation(&self) -> u64 {
        self.detach_generation
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        if self.prefix {
            self.prefix = false;
            if key_matches_byte(key, self.session.prefix_key) {
                self.send_prefix();
                return Ok(());
            }
            if let Some(bound) = self.bound_command_for_key(key) {
                return self.run_bound_command(bound);
            }
            return match prefix_command(key) {
                PrefixCommand::SplitHorizontal => self.split_active(SplitAxis::Horizontal, None),
                PrefixCommand::SplitVertical => self.split_active(SplitAxis::Vertical, None),
                PrefixCommand::NewWindow => self.create_window(None, None),
                PrefixCommand::NextPane => {
                    self.active_window_mut().next_pane();
                    Ok(())
                }
                PrefixCommand::PreviousPane => {
                    self.active_window_mut().previous_pane();
                    Ok(())
                }
                PrefixCommand::LastWindow => {
                    if !self.last_window() {
                        self.message("no last window");
                    }
                    Ok(())
                }
                PrefixCommand::ToggleLayout => {
                    self.toggle_layout();
                    Ok(())
                }
                PrefixCommand::Resize(direction) => {
                    if !self.resize_active_pane(direction, 1) {
                        self.message("pane cannot be resized");
                    }
                    Ok(())
                }
                PrefixCommand::SwapPane(direction) => {
                    if !self.swap_active_pane(direction) {
                        self.message("pane cannot be swapped");
                    }
                    Ok(())
                }
                PrefixCommand::KillPane => {
                    if !self.active_window_mut().kill_active_pane() {
                        self.message("last pane kept alive");
                    }
                    Ok(())
                }
                PrefixCommand::Detach => {
                    self.exit_reason = Some(ExitReason::Detach);
                    Ok(())
                }
                PrefixCommand::Quit => {
                    self.exit_reason = Some(ExitReason::Quit);
                    Ok(())
                }
                PrefixCommand::SelectWindow(ch) => {
                    self.select_window_digit(ch);
                    Ok(())
                }
                PrefixCommand::Unknown => {
                    self.message("unknown prefix command");
                    Ok(())
                }
            };
        }

        if key_matches_byte(key, self.session.prefix_key) {
            self.prefix = true;
            return Ok(());
        }

        if let Some(shortcut) = terminal_shortcut(key) {
            return self.run_terminal_shortcut(shortcut);
        }

        let bytes = encode_key(key);
        if !bytes.is_empty() {
            self.write_active(&bytes);
        }

        Ok(())
    }

    pub fn write_active(&self, bytes: &[u8]) {
        self.active_window().write_input(bytes);
    }

    pub fn paste_active(&self, bytes: &[u8]) {
        self.active_window().paste_input(bytes);
    }

    pub fn handle_mouse(&mut self, mouse: WireMouse) -> bool {
        if self.forward_mouse(mouse) {
            return true;
        }
        if matches!(mouse.kind, WireMouseKind::Down(WireMouseButton::Left)) {
            if mouse.row >= self.tty_size.rows.saturating_sub(STATUS_ROWS) {
                return self.select_window_at_status(mouse.col);
            }
            return self.select_pane_at(mouse.col, mouse.row);
        }
        false
    }

    /// Map a click on the status bar to the window tab under the cursor.
    fn select_window_at_status(&mut self, col: u16) -> bool {
        let mut x = text_width(&crate::render::status_session_label(&self.session.name));
        for index in 0..self.session.windows.len() {
            let window = &self.session.windows[index];
            let label = crate::render::status_window_label(index, &window.name, window.id.0);
            let end = x.saturating_add(text_width(&label));
            if col >= x && col < end {
                return self.session.select_window(index);
            }
            x = end;
        }
        false
    }

    pub fn send_prefix(&self) {
        self.write_active(&[self.session.prefix_key]);
    }

    pub fn new_window(&mut self, name: Option<String>, command: Option<&str>) -> Result<()> {
        self.create_window(name, command)
    }

    pub fn split_window(&mut self, axis: SplitAxis, command: Option<&str>) -> Result<()> {
        self.split_active(axis, command)
    }

    pub fn select_window(&mut self, index: usize) -> bool {
        self.session.select_window(index)
    }

    pub fn next_window(&mut self) {
        self.session.next_window();
    }

    pub fn previous_window(&mut self) {
        self.session.previous_window();
    }

    pub fn last_window(&mut self) -> bool {
        self.session.last_window()
    }

    pub fn select_pane(&mut self, index: usize) -> bool {
        self.active_window_mut().select_pane(index)
    }

    pub fn select_pane_at(&mut self, col: u16, row: u16) -> bool {
        if row >= self.tty_size.rows.saturating_sub(STATUS_ROWS) {
            return false;
        }

        let area = Rect {
            x: 0,
            y: 0,
            width: self.tty_size.cols,
            height: self.tty_size.rows.saturating_sub(STATUS_ROWS),
        };
        let layouts = self.active_window().layout_rects(area);
        let Some(index) = layouts
            .into_iter()
            .position(|rect| rect_contains(rect, col, row))
        else {
            return false;
        };

        self.select_pane(index)
    }

    fn forward_mouse(&mut self, mouse: WireMouse) -> bool {
        let Some((pane_index, position)) = self.mouse_target(mouse.col, mouse.row) else {
            return false;
        };
        let Some(event) = terminal_mouse_event(mouse.kind) else {
            return false;
        };

        let mode = match self.active_window().panes.get(pane_index) {
            Some(pane) => pane.terminal.mouse_mode(),
            None => return false,
        };
        let Some(packet) = encode_mouse_report(
            mode,
            event,
            position,
            terminal_mouse_modifiers(mouse.modifiers),
        ) else {
            return false;
        };

        self.active_window_mut().select_pane(pane_index);
        self.active_window().active_pane().write(&packet);
        true
    }

    fn mouse_target(&self, col: u16, row: u16) -> Option<(usize, TerminalMousePosition)> {
        if row >= self.tty_size.rows.saturating_sub(STATUS_ROWS) {
            return None;
        }

        let area = Rect {
            x: 0,
            y: 0,
            width: self.tty_size.cols,
            height: self.tty_size.rows.saturating_sub(STATUS_ROWS),
        };
        let layouts = self.active_window().layout_rects(area);
        let framed = layouts.len() > 1;
        let inset = u16::from(framed);
        layouts.into_iter().enumerate().find_map(|(index, rect)| {
            if !rect_contains(rect, col, row) {
                return None;
            }
            let inner_x = rect.x.saturating_add(inset);
            let inner_y = rect.y.saturating_add(inset);
            let inner_width = rect.width.saturating_sub(inset * 2);
            let inner_height = rect.height.saturating_sub(inset * 2);
            if col < inner_x
                || row < inner_y
                || col >= inner_x.saturating_add(inner_width)
                || row >= inner_y.saturating_add(inner_height)
            {
                return None;
            }
            Some((
                index,
                TerminalMousePosition {
                    col: usize::from(col - inner_x),
                    row: usize::from(row - inner_y),
                },
            ))
        })
    }

    pub fn next_pane(&mut self) {
        self.active_window_mut().next_pane();
    }

    pub fn previous_pane(&mut self) {
        self.active_window_mut().previous_pane();
    }

    pub fn select_layout(&mut self, axis: SplitAxis) {
        self.active_window_mut().select_layout(axis);
    }

    pub fn set_synchronize_panes(&mut self, enabled: bool) {
        self.active_window_mut().synchronize_panes = enabled;
    }

    pub fn set_default_command(&mut self, command: Option<String>) {
        self.session.default_command = command;
    }

    pub fn set_prefix_key(&mut self, key: u8) {
        self.session.prefix_key = key;
    }

    pub fn bind_key(&mut self, key: char, command: BoundCommand) {
        if let Some((_, existing)) = self
            .session
            .key_bindings
            .iter_mut()
            .find(|(bound_key, _)| *bound_key == key)
        {
            *existing = command;
        } else {
            self.session.key_bindings.push((key, command));
        }
    }

    pub fn resize_active_pane(&mut self, direction: ResizeDirection, amount: u16) -> bool {
        self.active_window_mut()
            .resize_active_pane(direction, amount)
    }

    pub fn swap_active_pane(&mut self, direction: SwapDirection) -> bool {
        self.active_window_mut().swap_active_pane(direction)
    }

    pub fn swap_active_window(&mut self, direction: SwapDirection) -> bool {
        self.session.swap_active_window(direction)
    }

    pub fn move_active_window(&mut self, index: usize) -> bool {
        self.session.move_active_window(index)
    }

    pub fn break_active_pane(&mut self, name: Option<String>) -> bool {
        let Some((pane, weight)) = self.active_window_mut().take_active_pane() else {
            return false;
        };
        let window_id = WindowId(self.next_window_id);
        self.next_window_id += 1;
        let name = name.unwrap_or_else(|| format!("win{}", window_id.0));
        self.session
            .push_window(Window::from_pane(window_id, name, pane, weight));
        true
    }

    pub fn join_pane(&mut self, source_window: usize, source_pane: usize) -> bool {
        self.session
            .join_pane_into_active(source_window, source_pane)
    }

    pub fn detach_clients(&mut self) {
        self.detach_generation = self.detach_generation.saturating_add(1);
    }

    pub fn kill_active_pane(&mut self) -> bool {
        self.active_window_mut().kill_active_pane()
    }

    pub fn kill_other_panes(&mut self) -> bool {
        self.active_window_mut().kill_other_panes()
    }

    pub fn respawn_active_pane(&mut self, command: Option<&str>) -> Result<()> {
        let effective_command = self.effective_command(command);
        let terminal_config = self.terminal_config.clone();
        self.active_window_mut()
            .respawn_active_pane(&terminal_config, effective_command.as_deref())
    }

    pub fn respawn_active_window(&mut self, command: Option<&str>) -> Result<()> {
        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let pane_cols = max(MIN_PANE_COLS, self.tty_size.cols.saturating_sub(2));
        let pane_rows = max(
            MIN_PANE_ROWS,
            self.tty_size.rows.saturating_sub(STATUS_ROWS + 2),
        );
        let size = self.terminal_config.size(pane_cols, pane_rows);
        let effective_command = self.effective_command(command);
        let terminal_config = self.terminal_config.clone();
        self.active_window_mut().respawn(
            pane_id,
            size,
            &terminal_config,
            effective_command.as_deref(),
        )
    }

    pub fn kill_active_window(&mut self) -> bool {
        self.session.kill_active_window()
    }

    pub fn rename_active_window(&mut self, name: impl Into<String>) {
        self.active_window_mut().name = name.into();
    }

    pub fn rename_session(&mut self, name: impl Into<String>) {
        self.session.name = name.into();
    }

    pub fn drain_events(&mut self) {
        let mut exited = Vec::new();
        for (window_index, window) in self.session.windows.iter().enumerate() {
            for (pane_index, pane) in window.panes.iter().enumerate() {
                let mut host = EmptyReplyHost;
                loop {
                    let (events, has_more) = pane.terminal.drain_events(&mut host);
                    if events
                        .iter()
                        .any(|event| matches!(event, TerminalEvent::Exit))
                    {
                        exited.push((window_index, pane_index));
                    }
                    if !has_more {
                        break;
                    }
                }
            }
        }
        exited.sort_unstable();
        exited.dedup();
        for (window_index, pane_index) in exited.into_iter().rev() {
            self.close_exited_pane(window_index, pane_index);
        }
    }

    fn active_window(&self) -> &Window {
        self.session.active_window()
    }

    fn active_window_mut(&mut self) -> &mut Window {
        self.session.active_window_mut()
    }

    fn bound_command_for_key(&self, key: KeyEvent) -> Option<BoundCommand> {
        let KeyCode::Char(ch) = key.code else {
            return None;
        };
        if !key.modifiers.is_empty() {
            return None;
        }
        self.session
            .key_bindings
            .iter()
            .find_map(|(bound_key, command)| (*bound_key == ch).then_some(*command))
    }

    fn run_bound_command(&mut self, command: BoundCommand) -> Result<()> {
        match command {
            BoundCommand::SplitHorizontal => self.split_active(SplitAxis::Horizontal, None),
            BoundCommand::SplitVertical => self.split_active(SplitAxis::Vertical, None),
            BoundCommand::NewWindow => self.create_window(None, None),
            BoundCommand::NextPane => {
                self.active_window_mut().next_pane();
                Ok(())
            }
            BoundCommand::PreviousPane => {
                self.active_window_mut().previous_pane();
                Ok(())
            }
            BoundCommand::KillPane => {
                if !self.active_window_mut().kill_active_pane() {
                    self.message("last pane kept alive");
                }
                Ok(())
            }
            BoundCommand::DetachClient => {
                self.exit_reason = Some(ExitReason::Detach);
                Ok(())
            }
            BoundCommand::LastWindow => {
                if !self.last_window() {
                    self.message("no last window");
                }
                Ok(())
            }
            BoundCommand::ToggleLayout => {
                self.toggle_layout();
                Ok(())
            }
        }
    }

    fn run_terminal_shortcut(&mut self, shortcut: TerminalShortcut) -> Result<()> {
        match shortcut {
            TerminalShortcut::Split(axis) => self.split_active(axis, None),
            TerminalShortcut::ClosePane => {
                if !self.kill_active_pane() {
                    self.message("last pane kept alive");
                }
                Ok(())
            }
            TerminalShortcut::NewWindow => self.create_window(None, None),
            TerminalShortcut::SelectWindow(index) => {
                if !self.select_window(index) {
                    self.message(format!("window {} does not exist", index + 1));
                }
                Ok(())
            }
        }
    }

    fn close_exited_pane(&mut self, window_index: usize, pane_index: usize) {
        let Some(window) = self.session.windows.get_mut(window_index) else {
            return;
        };
        if window.kill_pane(pane_index) {
            return;
        }
        if pane_index >= window.panes.len() {
            return;
        }
        if self.session.kill_window(window_index) {
            return;
        }
        self.exit_reason = Some(ExitReason::Quit);
    }

    fn split_active(&mut self, axis: SplitAxis, command: Option<&str>) -> Result<()> {
        let pane_id = PaneId(self.next_pane_id);
        self.next_pane_id += 1;
        let size = self.active_window().active_pane().terminal.size();
        let effective_command = self.effective_command(command);
        let terminal_config = self.terminal_config.clone();
        self.active_window_mut()
            .split(
                axis,
                pane_id,
                size,
                &terminal_config,
                effective_command.as_deref(),
            )
            .with_context(|| format!("split pane {pane_id:?}"))?;
        Ok(())
    }

    fn create_window(&mut self, name: Option<String>, command: Option<&str>) -> Result<()> {
        if self.session.windows.len() >= MAX_WINDOWS_PER_SESSION {
            return Err(anyhow!(
                "window limit reached (max {MAX_WINDOWS_PER_SESSION})"
            ));
        }
        let window_id = WindowId(self.next_window_id);
        let pane_id = PaneId(self.next_pane_id);
        self.next_window_id += 1;
        self.next_pane_id += 1;
        let pane_cols = max(MIN_PANE_COLS, self.tty_size.cols.saturating_sub(2));
        let pane_rows = max(
            MIN_PANE_ROWS,
            self.tty_size.rows.saturating_sub(STATUS_ROWS + 2),
        );
        let effective_command = self.effective_command(command);
        let mut window = Window::new(
            window_id,
            pane_id,
            self.terminal_config.size(pane_cols, pane_rows),
            &self.terminal_config,
            effective_command.as_deref(),
        )?;
        if let Some(name) = name {
            window.name = name;
        }
        self.session.push_window(window);
        Ok(())
    }

    fn effective_command(&self, command: Option<&str>) -> Option<String> {
        command
            .map(ToOwned::to_owned)
            .or_else(|| self.session.default_command.clone())
    }

    fn toggle_layout(&mut self) {
        let next = match self.active_window().split_axis {
            SplitAxis::Horizontal => SplitAxis::Vertical,
            SplitAxis::Vertical => SplitAxis::Horizontal,
        };
        self.select_layout(next);
    }

    fn select_window_digit(&mut self, digit: char) {
        if let Some(index) = digit.to_digit(10).and_then(|n| n.checked_sub(1)) {
            self.session.select_window(index as usize);
        }
    }

    fn message(&mut self, message: impl Into<String>) {
        if self.messages.len() > 4 {
            self.messages.pop_front();
        }
        self.messages.push_back(message.into());
    }
}

struct EmptyReplyHost;

impl TerminalReplyHost for EmptyReplyHost {
    fn load_clipboard(&mut self, _target: termy_core::TerminalClipboardTarget) -> Option<String> {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefixCommand {
    SplitHorizontal,
    SplitVertical,
    NewWindow,
    NextPane,
    PreviousPane,
    LastWindow,
    ToggleLayout,
    Resize(ResizeDirection),
    SwapPane(SwapDirection),
    KillPane,
    Detach,
    Quit,
    SelectWindow(char),
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalShortcut {
    Split(SplitAxis),
    ClosePane,
    NewWindow,
    SelectWindow(usize),
}

pub fn prefix_command(key: KeyEvent) -> PrefixCommand {
    match key.code {
        KeyCode::Char('|') | KeyCode::Char('%') => PrefixCommand::SplitHorizontal,
        KeyCode::Char('-') | KeyCode::Char('"') => PrefixCommand::SplitVertical,
        KeyCode::Char('c') => PrefixCommand::NewWindow,
        KeyCode::Char('n') | KeyCode::Right => PrefixCommand::NextPane,
        KeyCode::Char('p') | KeyCode::Left => PrefixCommand::PreviousPane,
        KeyCode::Char('l') => PrefixCommand::LastWindow,
        KeyCode::Char(' ') => PrefixCommand::ToggleLayout,
        KeyCode::Char('H') => PrefixCommand::Resize(ResizeDirection::Left),
        KeyCode::Char('L') => PrefixCommand::Resize(ResizeDirection::Right),
        KeyCode::Char('K') => PrefixCommand::Resize(ResizeDirection::Up),
        KeyCode::Char('J') => PrefixCommand::Resize(ResizeDirection::Down),
        KeyCode::Char('{') => PrefixCommand::SwapPane(SwapDirection::Previous),
        KeyCode::Char('}') => PrefixCommand::SwapPane(SwapDirection::Next),
        KeyCode::Char('x') => PrefixCommand::KillPane,
        KeyCode::Char('d') => PrefixCommand::Detach,
        KeyCode::Char('q') => PrefixCommand::Quit,
        KeyCode::Char(ch) if ch.is_ascii_digit() => PrefixCommand::SelectWindow(ch),
        _ => PrefixCommand::Unknown,
    }
}

fn terminal_shortcut(key: KeyEvent) -> Option<TerminalShortcut> {
    let KeyCode::Char(ch) = key.code else {
        return None;
    };

    let has_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let has_super = key.modifiers.contains(KeyModifiers::SUPER);
    let has_alt = key.modifiers.contains(KeyModifiers::ALT);
    let has_shift = key.modifiers.contains(KeyModifiers::SHIFT);

    if !(has_ctrl || has_super || has_alt) {
        return None;
    }

    if ch.eq_ignore_ascii_case(&'w') && (has_ctrl || has_super) {
        return Some(TerminalShortcut::ClosePane);
    }

    if ch.eq_ignore_ascii_case(&'t') && (has_ctrl || has_super) {
        return Some(TerminalShortcut::NewWindow);
    }

    if (has_ctrl || has_super)
        && let Some(index) = shortcut_window_index(ch)
    {
        return Some(TerminalShortcut::SelectWindow(index as usize));
    }

    if has_alt
        && !has_ctrl
        && !has_super
        && let Some(index) = shortcut_window_index(ch)
    {
        return Some(TerminalShortcut::SelectWindow(index as usize));
    }

    if ch.eq_ignore_ascii_case(&'d') && (has_ctrl || has_super) {
        let axis = if has_shift {
            SplitAxis::Vertical
        } else {
            SplitAxis::Horizontal
        };
        return Some(TerminalShortcut::Split(axis));
    }

    None
}

fn shortcut_window_index(ch: char) -> Option<u32> {
    let digit = match ch {
        '!' => 1,
        '@' => 2,
        '#' => 3,
        '$' => 4,
        '%' => 5,
        '^' => 6,
        '&' => 7,
        '*' => 8,
        '(' => 9,
        ')' => 0,
        ch => ch.to_digit(10)?,
    };
    digit.checked_sub(1)
}

pub fn encode_key(key: KeyEvent) -> Vec<u8> {
    match key.code {
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            control_byte(ch).into_iter().collect()
        }
        KeyCode::Char(ch) => {
            let mut buf = [0; 4];
            ch.encode_utf8(&mut buf).as_bytes().to_vec()
        }
        KeyCode::Enter => b"\r".to_vec(),
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => b"\t".to_vec(),
        KeyCode::Esc => b"\x1b".to_vec(),
        KeyCode::Up => b"\x1b[A".to_vec(),
        KeyCode::Down => b"\x1b[B".to_vec(),
        KeyCode::Right => b"\x1b[C".to_vec(),
        KeyCode::Left => b"\x1b[D".to_vec(),
        KeyCode::Home => b"\x1b[H".to_vec(),
        KeyCode::End => b"\x1b[F".to_vec(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        _ => Vec::new(),
    }
}

fn text_width(text: &str) -> u16 {
    unicode_width::UnicodeWidthStr::width(text).min(usize::from(u16::MAX)) as u16
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

fn control_byte(ch: char) -> Option<u8> {
    let lower = ch.to_ascii_lowercase();
    if lower.is_ascii_lowercase() {
        Some((lower as u8) - b'a' + 1)
    } else if ch == '[' {
        Some(0x1b)
    } else {
        None
    }
}

fn key_matches_byte(key: KeyEvent, byte: u8) -> bool {
    if let KeyCode::Char(ch) = key.code
        && key.modifiers.contains(KeyModifiers::CONTROL)
    {
        return control_byte(ch) == Some(byte);
    }
    false
}

fn format_key_byte(byte: u8) -> String {
    match byte {
        1..=26 => format!("C-{}", (b'a' + byte - 1) as char),
        0x1b => "C-[".to_string(),
        _ => format!("0x{byte:02x}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bracketed_paste_is_framed_once() {
        assert_eq!(
            framed_bracketed_paste_input("hello\nworld".as_bytes()),
            b"\x1b[200~hello\nworld\x1b[201~"
        );
    }

    #[test]
    fn bracketed_paste_strips_embedded_markers() {
        assert_eq!(
            framed_bracketed_paste_input(b"before\x1b[201~middle\x1b[200~after"),
            b"\x1b[200~beforemiddleafter\x1b[201~"
        );
    }
}
