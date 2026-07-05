use ratatui::{
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};
use termy_core::{TermyCell, TermyColor, TermyFrame};
use unicode_width::UnicodeWidthChar;

#[cfg(test)]
use crate::protocol::SplitAxis;
use crate::{
    STATUS_ROWS,
    protocol::{CellColor, PaneCell, PaneView, SessionView},
};

pub fn draw_view(frame: &mut ratatui::Frame<'_>, view: &SessionView) {
    let area = frame.area();
    let status_y = area.height.saturating_sub(STATUS_ROWS);
    let pane_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height.saturating_sub(STATUS_ROWS),
    };
    let status_area = Rect {
        x: area.x,
        y: area.y + status_y,
        width: area.width,
        height: STATUS_ROWS,
    };

    let framed = view.panes.len() > 1;
    for (index, pane) in view.panes.iter().enumerate() {
        let rect = pane.rect_or(pane_area);
        let is_active = index == view.active_pane;
        let paragraph = if framed {
            let border_style = if is_active {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Black)
            };
            Paragraph::new(pane_terminal_lines(pane)).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style),
            )
        } else {
            Paragraph::new(pane_terminal_lines(pane))
        };
        frame.render_widget(paragraph, rect);
        if is_active {
            if let Some(cursor) = pane.cursor {
                let offset = u16::from(framed);
                let cursor_x =
                    rect.x.saturating_add(offset).saturating_add(
                        cursor.col.min(usize::from(pane.cols.saturating_sub(1))) as u16,
                    );
                let cursor_y =
                    rect.y.saturating_add(offset).saturating_add(
                        cursor.row.min(usize::from(pane.rows.saturating_sub(1))) as u16,
                    );
                let right_limit = rect
                    .x
                    .saturating_add(rect.width.saturating_sub(u16::from(framed)));
                let bottom_limit = rect
                    .y
                    .saturating_add(rect.height.saturating_sub(u16::from(framed)));
                if cursor_x < right_limit && cursor_y < bottom_limit {
                    frame.set_cursor_position(Position::new(cursor_x, cursor_y));
                }
            }
        }
    }

    frame.render_widget(status_bar_view(view), status_area);
}

impl PaneView {
    fn rect_or(&self, fallback: Rect) -> Rect {
        if self.width == 0 || self.height == 0 {
            return fallback;
        }
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

#[cfg(test)]
pub fn compute_pane_rects(
    area: Rect,
    pane_count: usize,
    axis: SplitAxis,
    weights: &[u16],
) -> Vec<Rect> {
    if pane_count == 0 {
        return Vec::new();
    }

    let weights = normalized_weights(pane_count, weights);
    let mut rects = Vec::with_capacity(pane_count);
    match axis {
        SplitAxis::Horizontal => {
            let mut x = area.x;
            let mut remaining = area.width;
            let mut remaining_weight = weights.iter().sum::<u16>();
            for index in 0..pane_count {
                let width = if index + 1 == pane_count {
                    area.x + area.width - x
                } else {
                    weighted_span(remaining, weights[index], remaining_weight)
                };
                rects.push(Rect {
                    x,
                    y: area.y,
                    width,
                    height: area.height,
                });
                x = x.saturating_add(width);
                remaining = remaining.saturating_sub(width);
                remaining_weight = remaining_weight.saturating_sub(weights[index]);
            }
        }
        SplitAxis::Vertical => {
            let mut y = area.y;
            let mut remaining = area.height;
            let mut remaining_weight = weights.iter().sum::<u16>();
            for index in 0..pane_count {
                let height = if index + 1 == pane_count {
                    area.y + area.height - y
                } else {
                    weighted_span(remaining, weights[index], remaining_weight)
                };
                rects.push(Rect {
                    x: area.x,
                    y,
                    width: area.width,
                    height,
                });
                y = y.saturating_add(height);
                remaining = remaining.saturating_sub(height);
                remaining_weight = remaining_weight.saturating_sub(weights[index]);
            }
        }
    }
    rects
}

#[cfg(test)]
fn normalized_weights(pane_count: usize, weights: &[u16]) -> Vec<u16> {
    if weights.len() == pane_count && weights.iter().all(|weight| *weight > 0) {
        weights.to_vec()
    } else {
        vec![1; pane_count]
    }
}

#[cfg(test)]
fn weighted_span(remaining: u16, weight: u16, total_weight: u16) -> u16 {
    if total_weight == 0 {
        return 0;
    }
    let scaled = u32::from(remaining) * u32::from(weight) / u32::from(total_weight);
    scaled.max(1).min(u32::from(remaining)) as u16
}

#[cfg(test)]
pub fn frame_cells(frame: &TermyFrame, max_width: usize) -> Vec<PaneCell> {
    frame_cells_and_text_lines(frame, max_width).0
}

pub fn frame_cells_and_text_lines(
    frame: &TermyFrame,
    max_width: usize,
) -> (Vec<PaneCell>, Vec<String>) {
    let cols = usize::from(frame.cols);
    let rows = usize::from(frame.rows);
    let max_width = max_width.min(cols);
    let mut grid = vec![' '; rows * max_width];
    let mut cells = Vec::new();

    for cell in &frame.cells {
        if cell.row < rows && cell.col < max_width && cell.render_text {
            let ch = display_char(cell);
            grid[cell.row * max_width + cell.col] = ch;
            cells.push(PaneCell {
                col: cell.col,
                row: cell.row,
                ch,
                fg: cell_color(cell.fg),
                bg: cell_color(cell.bg),
                bold: cell.bold,
            });
        }
    }

    let lines = (0..rows)
        .map(|row| {
            let mut out = String::new();
            let mut width = 0;
            for col in 0..max_width {
                let ch = grid[row * max_width + col];
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(1);
                if width + ch_width > max_width {
                    break;
                }
                out.push(ch);
                width += ch_width;
            }
            out
        })
        .collect();

    (cells, lines)
}

/// Label for the session segment at the left of the status bar.
/// Must stay in sync with click hit-testing in `model::Rmux::select_window_at_status`.
pub fn status_session_label(session: &str) -> String {
    format!(" {session} ")
}

/// Label for one window tab in the status bar.
/// Must stay in sync with click hit-testing in `model::Rmux::select_window_at_status`.
pub fn status_window_label(index: usize, name: &str, id: u64) -> String {
    format!(" {}:{}#{} ", index + 1, name, id)
}

fn status_bar_view(view: &SessionView) -> Paragraph<'_> {
    let mut spans = Vec::new();
    spans.push(Span::styled(
        status_session_label(&view.session),
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD),
    ));
    for (index, window) in view.windows.iter().enumerate() {
        let style = if index == view.active_window {
            Style::default()
                .fg(Color::White)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Gray).bg(Color::Black)
        };
        spans.push(Span::styled(
            status_window_label(index, &window.name, window.id),
            style,
        ));
    }
    spans.push(Span::raw(" "));
    if view.prefix {
        spans.push(Span::styled(
            "PREFIX ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    if let Some(message) = &view.message {
        spans.push(Span::raw(message.clone()));
    }

    Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Black))
}

fn display_char(cell: &TermyCell) -> char {
    if cell.char.is_control() {
        ' '
    } else {
        cell.char
    }
}

fn cell_color(color: TermyColor) -> CellColor {
    CellColor {
        r: color.r,
        g: color.g,
        b: color.b,
    }
}

fn pane_terminal_lines(pane: &PaneView) -> Vec<Line<'static>> {
    if pane.cells.is_empty() {
        return pane
            .lines
            .iter()
            .cloned()
            .map(Line::from)
            .collect::<Vec<_>>();
    }

    let cols = usize::from(pane.cols);
    let rows = usize::from(pane.rows);
    let mut grid = vec![None; rows.saturating_mul(cols)];
    for cell in &pane.cells {
        if cell.row < rows && cell.col < cols {
            grid[cell.row * cols + cell.col] = Some(cell);
        }
    }

    (0..rows)
        .map(|row| {
            let mut spans = Vec::new();
            let mut current_style = None;
            let mut current_text = String::new();
            let mut occupied_until = 0usize;

            for col in 0..cols {
                if col < occupied_until {
                    continue;
                }
                let cell = grid[row * cols + col];
                let style = cell.map_or(CellStyle::default(), CellStyle::from);
                let ch = cell.map_or(' ', |cell| cell.ch);
                if current_style != Some(style) {
                    flush_span(&mut spans, &mut current_text, current_style);
                    current_style = Some(style);
                }
                current_text.push(ch);
                occupied_until = col + UnicodeWidthChar::width(ch).unwrap_or(1).max(1);
            }

            flush_span(&mut spans, &mut current_text, current_style);
            Line::from(spans)
        })
        .collect()
}

fn flush_span(spans: &mut Vec<Span<'static>>, text: &mut String, style: Option<CellStyle>) {
    if text.is_empty() {
        return;
    }
    let text = std::mem::take(text);
    spans.push(Span::styled(text, style.unwrap_or_default().rat_style()));
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CellStyle {
    fg: Option<CellColor>,
    bg: Option<CellColor>,
    bold: bool,
}

impl From<&PaneCell> for CellStyle {
    fn from(cell: &PaneCell) -> Self {
        Self {
            fg: Some(cell.fg),
            bg: Some(cell.bg),
            bold: cell.bold,
        }
    }
}

impl CellStyle {
    fn rat_style(self) -> Style {
        let mut style = Style::default();
        if let Some(fg) = self.fg {
            style = style.fg(Color::Rgb(fg.r, fg.g, fg.b));
        }
        if let Some(bg) = self.bg {
            style = style.bg(Color::Rgb(bg.r, bg.g, bg.b));
        }
        if self.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        style
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::CursorView;
    use termy_core::{TermyCell, TermyColor, TermyFrame};

    #[test]
    fn frame_cells_preserve_terminal_style() {
        let frame = TermyFrame {
            cols: 1,
            rows: 1,
            cells: vec![TermyCell {
                col: 0,
                row: 0,
                char: 'x',
                fg: TermyColor {
                    r: 220,
                    g: 50,
                    b: 47,
                    a: 255,
                },
                bg: TermyColor {
                    r: 7,
                    g: 54,
                    b: 66,
                    a: 255,
                },
                uses_terminal_default_bg: false,
                bold: true,
                render_text: true,
            }],
            cursor: None,
            display_offset: 0,
            history_size: 0,
        };

        assert_eq!(
            frame_cells(&frame, 1),
            vec![PaneCell {
                col: 0,
                row: 0,
                ch: 'x',
                fg: CellColor {
                    r: 220,
                    g: 50,
                    b: 47,
                },
                bg: CellColor { r: 7, g: 54, b: 66 },
                bold: true,
            }]
        );
    }

    #[test]
    fn pane_terminal_lines_render_styled_cells() {
        let pane = PaneView {
            id: 1,
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            cols: 1,
            rows: 1,
            cells: vec![PaneCell {
                col: 0,
                row: 0,
                ch: 'x',
                fg: CellColor { r: 1, g: 2, b: 3 },
                bg: CellColor { r: 4, g: 5, b: 6 },
                bold: true,
            }],
            cursor: Some(CursorView { col: 0, row: 0 }),
            lines: vec!["fallback".to_string()],
        };

        let lines = pane_terminal_lines(&pane);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans[0].content.as_ref(), "x");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Rgb(1, 2, 3)));
        assert_eq!(lines[0].spans[0].style.bg, Some(Color::Rgb(4, 5, 6)));
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }
}
