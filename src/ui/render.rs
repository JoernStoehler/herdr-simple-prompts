use crate::agent::AgentStatus;
use crate::app::AppState;
use crate::composer::ComposerAccess;
use crate::editor::Editor;
use crate::markdown::is_safe_hyperlink_url;
use crate::style::AnsiColor;
use crate::ui::visual_rows::{
    CellStyle, HistoryDocument, TurnRenderCache, VisualRow, wrap_plain, wrap_styled,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::{Backend, TestBackend};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::{Constraint, Direction, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use std::io;
use std::time::Instant;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, Eq, PartialEq)]
struct HyperlinkArea {
    x: u16,
    y: u16,
    width: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HyperlinkPatch {
    area: HyperlinkArea,
    text: String,
    url: String,
}

#[derive(Default)]
struct RenderEffects {
    hyperlinks: Vec<HyperlinkPatch>,
    cursor: Option<Position>,
    /// The width the draft was wrapped to. Clearing back to the start of a line
    /// means the line as drawn, so the keys need to know where the rows broke.
    composer_width: usize,
}

fn horizontal_content_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(u16::from(area.width > 0)),
        y: area.y,
        width: area.width.saturating_sub(2),
        height: area.height,
    }
}

#[derive(Default)]
pub(crate) struct HistoryRenderCache {
    generation: u64,
    cached_generation: Option<u64>,
    width: Option<u16>,
    document: HistoryDocument,
    turns: TurnRenderCache,
    scroll_from_bottom: usize,
    maximum_offset: usize,
    viewport_height: Option<usize>,
    viewport_initialized: bool,
    terminal_hyperlinks: Vec<HyperlinkArea>,
    #[cfg(test)]
    rebuilds: usize,
}

impl HistoryRenderCache {
    pub(crate) fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    fn document_for(&mut self, app: &AppState, width: u16) -> &HistoryDocument {
        if self.cached_generation != Some(self.generation) || self.width != Some(width) {
            self.document = HistoryDocument::from_app_cached(app, width, &mut self.turns);
            self.cached_generation = Some(self.generation);
            self.width = Some(width);
            #[cfg(test)]
            {
                self.rebuilds += 1;
            }
        }
        &self.document
    }

    pub(crate) fn viewport_rows(
        &mut self,
        app: &AppState,
        width: u16,
        height: usize,
    ) -> Vec<VisualRow> {
        let rebuild = self.cached_generation != Some(self.generation) || self.width != Some(width);
        let layout_changed = self.viewport_height != Some(height);
        let old_maximum = self.maximum_offset;
        let old_offset = self.scroll_from_bottom.min(old_maximum);
        let old_top = old_maximum.saturating_sub(old_offset);
        let requested_initial_offset = app.scroll_from_bottom;

        self.document_for(app, width);
        let visible_height = height.min(self.document.rows.len());
        let new_maximum = self.document.rows.len().saturating_sub(visible_height);
        if !self.viewport_initialized {
            self.scroll_from_bottom = requested_initial_offset.min(new_maximum);
            self.viewport_initialized = true;
        } else if rebuild || layout_changed {
            self.scroll_from_bottom = if old_offset == 0 {
                0
            } else {
                new_maximum.saturating_sub(old_top.min(new_maximum))
            };
        } else {
            self.scroll_from_bottom = self.scroll_from_bottom.min(new_maximum);
        }
        self.maximum_offset = new_maximum;
        self.viewport_height = Some(height);
        self.document.viewport(height, self.scroll_from_bottom)
    }

    pub(crate) fn scroll_up(&mut self, rows: usize) {
        self.scroll_from_bottom = self
            .scroll_from_bottom
            .saturating_add(rows)
            .min(self.maximum_offset);
    }

    pub(crate) fn scroll_down(&mut self, rows: usize) {
        self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(rows);
    }

    pub(crate) fn scroll_from_bottom(&self) -> usize {
        self.scroll_from_bottom
    }

    pub(crate) fn maximum_offset(&self) -> usize {
        self.maximum_offset
    }

    /// A page keeps two rows of the previous view, so the eye has something to
    /// land on instead of starting again in unfamiliar text.
    pub(crate) fn page_rows(&self) -> usize {
        self.viewport_height.unwrap_or(1).saturating_sub(2).max(1)
    }

    pub(crate) fn scroll_to_oldest(&mut self) {
        self.scroll_from_bottom = self.maximum_offset;
    }

    pub(crate) fn scroll_to_latest(&mut self) {
        self.scroll_from_bottom = 0;
    }

    #[cfg(test)]
    fn rebuild_count(&self) -> usize {
        self.rebuilds
    }
}

fn render(
    frame: &mut Frame<'_>,
    app: &AppState,
    editor: &Editor,
    history_cache: &mut HistoryRenderCache,
) -> RenderEffects {
    if app.agent_status == AgentStatus::Blocked {
        render_blocked(frame, app);
        return RenderEffects::default();
    }
    let frame_area = frame.area();
    let area = horizontal_content_area(frame_area);
    let display_text = editor.display_text();
    let status_note = status_note(app);
    let working_height = u16::from(status_note.is_some());
    let composer_guard = app
        .input_enabled
        .then(|| match app.composer_access() {
            ComposerAccess::Ready => None,
            ComposerAccess::Occupied => {
                Some("Native composer contains unsent input · prefix+m to return")
            }
            ComposerAccess::Unknown => {
                Some("Unable to verify native composer · prefix+m to return")
            }
        })
        .flatten();
    let attachment_rows = if composer_guard.is_some() {
        0
    } else {
        attachment_visual_height(app, area.width)
    };
    let cells = usize::from(area.width.max(1));
    let wrapped_draft = wrap_plain(display_text, cells);
    let wrapped_guard = composer_guard.map(|warning| wrap_plain(warning, cells));
    let editor_rows = u16::try_from(wrapped_guard.as_ref().unwrap_or(&wrapped_draft).height())
        .unwrap_or(u16::MAX);
    let composer_rows = attachment_rows.saturating_add(editor_rows);
    let composer_height = (composer_rows + 1).clamp(3, (area.height * 2 / 5).max(3));
    let error_height = u16::from(app.visible_error().is_some());
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(error_height),
            Constraint::Length(working_height),
            Constraint::Length(composer_height),
            Constraint::Length(1),
        ])
        .split(area);

    let history_rows =
        history_cache.viewport_rows(app, areas[0].width, usize::from(areas[0].height));
    let scrolled_rows = history_cache.scroll_from_bottom();
    let scrollable_rows = history_cache.maximum_offset();
    let mut effects = RenderEffects {
        hyperlinks: hyperlink_patches(&history_rows, areas[0]),
        cursor: None,
        composer_width: cells,
    };
    let history = Text::from(
        history_rows
            .iter()
            .map(|row| visual_row_line(row, areas[0].width))
            .collect::<Vec<_>>(),
    );
    frame.render_widget(Paragraph::new(history), areas[0]);
    render_prompt_edge_fills(frame, frame_area, areas[0], &history_rows);
    render_history_scrollbar(
        frame,
        frame_area,
        areas[0],
        &history_rows,
        scrolled_rows,
        scrollable_rows,
    );
    if let Some(error) = app.visible_error() {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Error: ", Style::default().fg(Color::Red)),
                Span::raw(error),
            ])),
            areas[1],
        );
    }
    if let Some(note) = status_note {
        frame.render_widget(
            Paragraph::new(note).style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            areas[2],
        );
    }
    let mut composer_lines = Vec::new();
    if composer_guard.is_none() {
        // Confirmed attachments hold their own place in the line; only the ones
        // still being verified are announced above it.
        composer_lines.extend(app.pending_attachments.iter().enumerate().map(
            |(index, attachment)| {
                Line::styled(
                    format!(
                        "[Image #{}] {} (verifying…)",
                        app.draft_attachments.len() + index + 1,
                        attachment.display
                    ),
                    Style::default().fg(Color::Yellow),
                )
            },
        ));
    }
    if !app.input_enabled {
        composer_lines.push(Line::from(Span::styled(
            "Input disabled · reopen Simple Prompts",
            Style::default().fg(Color::Red),
        )));
    } else if let Some(wrapped) = wrapped_guard.as_ref() {
        composer_lines.extend(wrapped.rows.iter().map(|row| {
            Line::from(Span::styled(
                row.clone(),
                Style::default().fg(Color::Yellow),
            ))
        }));
    } else if display_text.is_empty() {
        composer_lines.push(Line::from(Span::styled(
            "Write a prompt",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let highlight = editor.attachment_span_at_cursor();
        composer_lines.extend(
            wrapped_draft
                .rows
                .iter()
                .enumerate()
                .map(|(index, row)| composer_row(row, wrapped_draft.row_start(index), highlight)),
        );
    }
    let composer = Text::from(composer_lines);
    let (editor_row, editor_column) = wrapped_draft.cell_of(editor.display_cursor_byte());
    let editor_row = u16::try_from(editor_row).unwrap_or(u16::MAX);
    let editor_column = u16::try_from(editor_column).unwrap_or(u16::MAX);
    let cursor_content_row = attachment_rows.saturating_add(editor_row);
    let visible_composer_rows = areas[3].height.saturating_sub(1).max(1);
    let composer_scroll = cursor_content_row.saturating_sub(visible_composer_rows - 1);
    frame.render_widget(
        Paragraph::new(composer)
            .block(Block::default().borders(Borders::TOP))
            .scroll((composer_scroll, 0)),
        areas[3],
    );
    frame.render_widget(Paragraph::new(footer(app, scrolled_rows)), areas[4]);

    if app.input_enabled && composer_guard.is_none() && areas[3].width > 0 {
        let (cursor_row, cursor_column) =
            editor_cursor(areas[3], cursor_content_row, editor_column, composer_scroll);
        let cursor = Position::new(cursor_column, cursor_row);
        frame.set_cursor_position(cursor);
        effects.cursor = Some(cursor);
    }
    effects
}

/// Draws one row of the draft, marking the image the cursor is sitting on.
///
/// The native composer highlights a marker whole rather than letting the cursor
/// walk through it, which is how it shows the thing is one piece. The overlay
/// treats it as one piece too, so it says so the same way.
fn composer_row(row: &str, row_start: usize, highlight: Option<(usize, usize)>) -> Line<'static> {
    let Some((start, end)) = highlight else {
        return Line::from(row.to_owned());
    };
    let row_end = row_start + row.len();
    let (start, end) = (start.max(row_start), end.min(row_end));
    if start >= end {
        return Line::from(row.to_owned());
    }
    let (start, end) = (start - row_start, end - row_start);
    if !row.is_char_boundary(start) || !row.is_char_boundary(end) {
        return Line::from(row.to_owned());
    }
    Line::from(vec![
        Span::raw(row[..start].to_owned()),
        Span::styled(
            row[start..end].to_owned(),
            Style::default().add_modifier(Modifier::REVERSED),
        ),
        Span::raw(row[end..].to_owned()),
    ])
}

/// The one line that says what is being waited for.
///
/// A pane asked to attach or drop an image answers in its own time, so the wait
/// is named and counted rather than left as a composer that does nothing.
fn status_note(app: &AppState) -> Option<String> {
    if let Some(pending) = &app.pending_action {
        let elapsed = Instant::now()
            .saturating_duration_since(pending.since)
            .as_secs();
        return Some(format!("{} ({elapsed}s)…", pending.label));
    }
    (app.agent_status == AgentStatus::Working).then(|| {
        let elapsed = app
            .working_since
            .map(|started| Instant::now().saturating_duration_since(started).as_secs())
            .unwrap_or(0);
        format!("Working ({elapsed}s · esc to interrupt)")
    })
}

fn render_prompt_edge_fills(
    frame: &mut Frame<'_>,
    frame_area: Rect,
    history_area: Rect,
    rows: &[VisualRow],
) {
    if frame_area.width < 3 {
        return;
    }
    for (offset, row) in rows.iter().enumerate() {
        let Some(fill) = row.fill else {
            continue;
        };
        let Ok(offset) = u16::try_from(offset) else {
            break;
        };
        let Some(y) = history_area.y.checked_add(offset) else {
            break;
        };
        if y >= history_area.bottom() {
            break;
        }
        let style = ratatui_style(fill);
        frame.buffer_mut()[(frame_area.x, y)].set_style(style);
        frame.buffer_mut()[(frame_area.right() - 1, y)].set_style(style);
    }
}

/// The scroll thumb, drawn in the outer gutter column.
///
/// Only the glyph and its color are set, so a prompt band keeps its background
/// underneath and still reaches both terminal edges; prompt text wraps one
/// column earlier so nothing of the conversation is ever hidden by the thumb.
/// It appears only when there is something outside the view to reach — a
/// history that fits says so by showing nothing, and leaves both gutters empty.
fn render_history_scrollbar(
    frame: &mut Frame<'_>,
    frame_area: Rect,
    history_area: Rect,
    rows: &[VisualRow],
    scrolled_rows: usize,
    scrollable_rows: usize,
) {
    if scrollable_rows == 0 || frame_area.width < 3 || history_area.height == 0 {
        return;
    }
    let track = usize::from(history_area.height);
    let thumb = (track * track / (track + scrollable_rows)).clamp(1, track);
    let travel = track - thumb;
    let rows_above = scrollable_rows - scrolled_rows.min(scrollable_rows);
    let top = rows_above * travel / scrollable_rows;
    let x = frame_area.right() - 1;
    for offset in 0..track {
        if rows.get(offset).is_none() {
            break;
        }
        let Some(y) = history_area
            .y
            .checked_add(u16::try_from(offset).unwrap_or(u16::MAX))
            .filter(|y| *y < history_area.bottom())
        else {
            break;
        };
        let on_thumb = offset >= top && offset < top + thumb;
        let cell = &mut frame.buffer_mut()[(x, y)];
        cell.set_symbol(if on_thumb { "▐" } else { "│" });
        cell.set_fg(if on_thumb {
            Color::Gray
        } else {
            Color::DarkGray
        });
    }
}

fn hyperlink_patches(rows: &[VisualRow], area: Rect) -> Vec<HyperlinkPatch> {
    let mut patches = Vec::new();
    for (row_offset, row) in rows.iter().enumerate() {
        let Ok(row_offset) = u16::try_from(row_offset) else {
            break;
        };
        let Some(y) = area
            .y
            .checked_add(row_offset)
            .filter(|y| *y < area.bottom())
        else {
            break;
        };
        let mut x = area.x;
        for span in &row.spans {
            let width = u16::try_from(UnicodeWidthStr::width(span.text.as_str()))
                .unwrap_or(u16::MAX)
                .min(area.right().saturating_sub(x));
            if let Some(url) = span.hyperlink.as_deref()
                && width > 0
                && is_safe_hyperlink_url(url)
                && !span.text.chars().any(char::is_control)
            {
                patches.push(HyperlinkPatch {
                    area: HyperlinkArea { x, y, width },
                    text: span.text.clone(),
                    url: url.to_owned(),
                });
            }
            x = x.saturating_add(width);
            if x >= area.right() {
                break;
            }
        }
    }
    patches
}

/// Draws the overlay and reports the width the draft was wrapped to.
pub(crate) fn draw_terminal<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &AppState,
    editor: &Editor,
    history_cache: &mut HistoryRenderCache,
) -> io::Result<usize> {
    let mut effects = RenderEffects::default();
    let cells = {
        let completed = terminal.draw(|frame| {
            effects = render(frame, app, editor, history_cache);
        })?;
        let mut cells = plain_cells(completed.buffer, &history_cache.terminal_hyperlinks);
        cells.extend(hyperlink_cells(completed.buffer, &effects.hyperlinks));
        cells
    };

    history_cache.terminal_hyperlinks = effects
        .hyperlinks
        .iter()
        .map(|patch| patch.area.clone())
        .collect();
    if cells.is_empty() {
        return Ok(effects.composer_width);
    }
    terminal
        .backend_mut()
        .draw(cells.iter().map(|(x, y, cell)| (*x, *y, cell)))?;
    if let Some(cursor) = effects.cursor {
        terminal.set_cursor_position(cursor)?;
    }
    terminal.backend_mut().flush()?;
    Ok(effects.composer_width)
}

fn plain_cells(buffer: &Buffer, areas: &[HyperlinkArea]) -> Vec<(u16, u16, Cell)> {
    let mut cells = Vec::new();
    for area in areas {
        let start = area.x.max(buffer.area.x);
        let end = area.x.saturating_add(area.width).min(buffer.area.right());
        let mut x = buffer.area.x;
        while x < end {
            let position = Position::new(x, area.y);
            let Some(cell) = buffer.cell(position) else {
                break;
            };
            let width = u16::try_from(UnicodeWidthStr::width(cell.symbol()))
                .unwrap_or(u16::MAX)
                .max(1);
            if x < start || cell.skip {
                x = x.saturating_add(width);
                continue;
            }
            if x >= end {
                break;
            }
            cells.push((x, area.y, cell.clone()));
            x = x.saturating_add(width);
        }
    }
    cells
}

fn hyperlink_cells(buffer: &Buffer, patches: &[HyperlinkPatch]) -> Vec<(u16, u16, Cell)> {
    patches
        .iter()
        .filter_map(|patch| {
            let mut cell = buffer
                .cell(Position::new(patch.area.x, patch.area.y))?
                .clone();
            cell.set_symbol(&format!(
                "\u{1b}]8;;{}\u{7}{}\u{1b}]8;;\u{7}",
                patch.url, patch.text
            ));
            Some((patch.area.x, patch.area.y, cell))
        })
        .collect()
}

fn render_blocked(frame: &mut Frame<'_>, app: &AppState) {
    let area = horizontal_content_area(frame.area());
    let error_height = u16::from(app.interaction_error.is_some());
    let areas = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(error_height),
            Constraint::Length(1),
        ])
        .split(area);
    frame.render_widget(
        Paragraph::new("INTERACTION REQUIRED").style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        areas[0],
    );

    let body = match app.blocked_surface.as_ref() {
        Some(Ok(surface)) => {
            let rows = wrap_styled(surface, usize::from(areas[1].width));
            let start = rows.len().saturating_sub(usize::from(areas[1].height));
            Text::from(
                rows[start..]
                    .iter()
                    .map(|row| visual_row_line(row, areas[1].width))
                    .collect::<Vec<_>>(),
            )
        }
        Some(Err(_)) | None => Text::from("Unable to read native interaction"),
    };
    frame.render_widget(Paragraph::new(body), areas[1]);
    if let Some(error) = &app.interaction_error {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Error: ", Style::default().fg(Color::Red)),
                Span::raw(error.clone()),
            ])),
            areas[2],
        );
    }
    frame.render_widget(
        Paragraph::new("Native Codex/Claude interaction · prefix+m to return"),
        areas[3],
    );
}

fn visual_row_line(row: &VisualRow, width: u16) -> Line<'static> {
    let fill = row.fill.unwrap_or_default();
    let mut spans = row
        .spans
        .iter()
        .map(|span| {
            Span::styled(
                span.text.clone(),
                ratatui_style(merge_styles(fill, span.style)),
            )
        })
        .collect::<Vec<_>>();
    let padding = usize::from(width).saturating_sub(row.cell_width());
    if padding > 0 && row.fill.is_some() {
        spans.push(Span::styled(" ".repeat(padding), ratatui_style(fill)));
    }
    Line::from(spans)
}

fn merge_styles(base: CellStyle, overlay: CellStyle) -> CellStyle {
    CellStyle {
        foreground: overlay.foreground.or(base.foreground),
        background: overlay.background.or(base.background),
        modifiers: crate::style::StyleModifiers {
            bold: base.modifiers.bold || overlay.modifiers.bold,
            dim: base.modifiers.dim || overlay.modifiers.dim,
            italic: base.modifiers.italic || overlay.modifiers.italic,
            underline: base.modifiers.underline || overlay.modifiers.underline,
            reverse: base.modifiers.reverse || overlay.modifiers.reverse,
        },
    }
}

fn ratatui_style(style: CellStyle) -> Style {
    let mut rendered = Style::default();
    if let Some(foreground) = style.foreground {
        rendered = rendered.fg(ratatui_color(foreground));
    }
    if let Some(background) = style.background {
        rendered = rendered.bg(ratatui_color(background));
    }
    if style.modifiers.bold {
        rendered = rendered.add_modifier(Modifier::BOLD);
    }
    if style.modifiers.dim {
        rendered = rendered.add_modifier(Modifier::DIM);
    }
    if style.modifiers.italic {
        rendered = rendered.add_modifier(Modifier::ITALIC);
    }
    if style.modifiers.underline {
        rendered = rendered.add_modifier(Modifier::UNDERLINED);
    }
    if style.modifiers.reverse {
        rendered = rendered.add_modifier(Modifier::REVERSED);
    }
    rendered
}

fn ratatui_color(color: AnsiColor) -> Color {
    match color {
        AnsiColor::Black => Color::Black,
        AnsiColor::Red => Color::Red,
        AnsiColor::Green => Color::Green,
        AnsiColor::Yellow => Color::Yellow,
        AnsiColor::Blue => Color::Blue,
        AnsiColor::Magenta => Color::Magenta,
        AnsiColor::Cyan => Color::Cyan,
        AnsiColor::White => Color::Gray,
        AnsiColor::BrightBlack => Color::DarkGray,
        AnsiColor::BrightRed => Color::LightRed,
        AnsiColor::BrightGreen => Color::LightGreen,
        AnsiColor::BrightYellow => Color::LightYellow,
        AnsiColor::BrightBlue => Color::LightBlue,
        AnsiColor::BrightMagenta => Color::LightMagenta,
        AnsiColor::BrightCyan => Color::LightCyan,
        AnsiColor::BrightWhite => Color::White,
        AnsiColor::Indexed(index) => Color::Indexed(index),
        AnsiColor::Rgb(red, green, blue) => Color::Rgb(red, green, blue),
    }
}

fn attachment_visual_height(app: &AppState, width: u16) -> u16 {
    let pending = app
        .pending_attachments
        .iter()
        .enumerate()
        .map(|(index, attachment)| {
            format!(
                "[Image #{}] {} (verifying…)",
                app.draft_attachments.len() + index + 1,
                attachment.display
            )
        });
    pending.fold(0_u16, |height, line| {
        let rows = wrap_plain(&line, usize::from(width.max(1))).height();
        height.saturating_add(u16::try_from(rows).unwrap_or(u16::MAX))
    })
}

/// The footer says where the view stands as well as what it is looking at.
///
/// A history scrolled away from the latest answer looks much like one that has
/// simply stopped growing, so the distance back and the key that closes it are
/// named first, where even a narrow pane still shows them.
fn footer(app: &AppState, scrolled_rows: usize) -> String {
    let mut fields = Vec::new();
    if scrolled_rows > 0 {
        fields.push(format!("↑ {scrolled_rows} rows · Shift+End for the latest"));
    }
    let Some(status) = &app.status_line else {
        fields.push("Simple Prompts · prefix+m to return".to_owned());
        return fields.join(" · ");
    };
    fields.push(status.agent.to_string());
    if let Some(model) = &status.model {
        fields.push(model.clone());
    }
    fields.push(status.cwd.display().to_string());
    if let Some(branch) = &status.branch {
        fields.push(branch.clone());
    }
    if let Some(usage) = &status.usage {
        fields.push(usage.clone());
    }
    fields.join(" · ")
}

fn editor_cursor(area: Rect, content_row: u16, column: u16, scroll: u16) -> (u16, u16) {
    (
        (area.y + 1 + content_row.saturating_sub(scroll)).min(area.bottom().saturating_sub(1)),
        (area.x + column).min(area.right().saturating_sub(1)),
    )
}

pub fn render_to_buffer(app: &AppState, editor: &Editor, width: u16, height: u16) -> Buffer {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut history_cache = HistoryRenderCache::default();
    terminal
        .draw(|frame| {
            render(frame, app, editor, &mut history_cache);
        })
        .unwrap();
    terminal.backend().buffer().clone()
}

pub fn render_terminal_to_buffer(
    app: &AppState,
    editor: &Editor,
    width: u16,
    height: u16,
) -> (Buffer, (u16, u16)) {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).unwrap();
    let mut history_cache = HistoryRenderCache::default();
    draw_terminal(&mut terminal, app, editor, &mut history_cache).unwrap();
    let cursor = terminal.get_cursor_position().unwrap();
    (terminal.backend().buffer().clone(), (cursor.x, cursor.y))
}

pub fn render_to_string(app: &AppState, editor: &Editor, width: u16, height: u16) -> String {
    let buffer = render_to_buffer(app, editor, width, height);
    let mut output = String::new();
    for y in 0..height {
        for x in 0..width {
            output.push_str(buffer[(x, y)].symbol());
        }
        output.push('\n');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{HistoryRenderCache, HyperlinkArea, hyperlink_patches, plain_cells};
    use crate::app::{AppEvent, AppState};
    use crate::model::Message;
    use crate::style::{AnsiColor, MessagePresentation, StyleModifiers, StyleRun, StyledText};
    use crate::ui::visual_rows::{CellStyle, VisualRow, VisualSpan};
    use ratatui::Terminal;
    use ratatui::backend::{CrosstermBackend, TestBackend};
    use ratatui::layout::Rect;
    use ratatui::{TerminalOptions, Viewport};
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct RecordingWriter(Arc<Mutex<Vec<u8>>>);

    impl RecordingWriter {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    impl Write for RecordingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// The composer draws, measures and places its cursor from one wrapping.
    ///
    /// They used to disagree: the text was drawn with word wrapping while the
    /// cursor divided by the width, so as soon as a word wrapped the cursor sat
    /// in empty space and the box came out a row short.
    #[test]
    fn the_composer_cursor_follows_the_drawn_wrapping() {
        let text = "aaaa bbbb cccccccccc dddd";
        let mut editor = crate::editor::Editor::default();
        editor.insert_paste(text);
        let app = AppState::default();

        let (buffer, (x, y)) = super::render_terminal_to_buffer(&app, &editor, 24, 12);

        let row: String = (0..24).map(|column| buffer[(column, y)].symbol()).collect();
        let before_cursor = row[..usize::from(x)].trim_end().to_owned();
        assert!(
            before_cursor.ends_with("dddd"),
            "the cursor must sit after the last character, row was {row:?}"
        );
    }

    #[test]
    fn history_cache_reuses_until_invalidated_or_resized() {
        let mut cache = HistoryRenderCache::default();
        let mut app = AppState::default();

        cache.document_for(&app, 20);
        cache.document_for(&app, 20);
        assert_eq!(cache.rebuild_count(), 1);

        app.apply(AppEvent::NativeUser(Message::text(
            "u1",
            "changed",
            Some(1),
        )));
        cache.invalidate();
        cache.document_for(&app, 20);
        assert_eq!(cache.rebuild_count(), 2);

        cache.document_for(&app, 21);
        assert_eq!(cache.rebuild_count(), 3);
    }

    #[test]
    fn history_scroll_clamps_overscroll_and_page_down_moves_immediately() {
        let mut cache = HistoryRenderCache::default();
        let mut app = AppState::default();
        for index in 0..8 {
            app.apply(AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
            app.apply(AppEvent::NativeFinal(Message::final_text(
                format!("a{index}"),
                format!("answer {index}"),
                Some(index),
            )));
        }
        let _ = cache.viewport_rows(&app, 40, 5);

        cache.scroll_up(usize::MAX);
        assert_eq!(cache.scroll_from_bottom(), cache.maximum_offset());
        let oldest = cache.scroll_from_bottom();
        cache.scroll_down(5);

        assert_eq!(cache.scroll_from_bottom(), oldest.saturating_sub(5));
    }

    #[test]
    fn history_append_preserves_the_manual_viewport_top() {
        let mut cache = HistoryRenderCache::default();
        let mut app = AppState::default();
        for index in 0..8 {
            app.apply(AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
            app.apply(AppEvent::NativeFinal(Message::final_text(
                format!("a{index}"),
                format!("answer {index}"),
                Some(index),
            )));
        }
        let _ = cache.viewport_rows(&app, 40, 5);
        cache.scroll_up(7);
        let before = cache
            .viewport_rows(&app, 40, 5)
            .into_iter()
            .map(|row| row.plain_text())
            .collect::<Vec<_>>();

        app.apply(AppEvent::NativeUser(Message::text(
            "u-new",
            "new prompt",
            Some(100),
        )));
        app.apply(AppEvent::NativeFinal(Message::final_text(
            "a-new",
            "new answer",
            Some(101),
        )));
        cache.invalidate();
        let after = cache
            .viewport_rows(&app, 40, 5)
            .into_iter()
            .map(|row| row.plain_text())
            .collect::<Vec<_>>();

        assert_eq!(after, before);
    }

    #[test]
    fn terminal_draw_clears_a_stale_link_when_metadata_disappears() {
        let mut app = AppState::default();
        app.apply(AppEvent::NativeUser(Message::text("u1", "prompt", None)));
        app.apply(AppEvent::NativeFinal(Message {
            stable_id: "a1".into(),
            text: "[docs](https://example.test)".into(),
            presentation: MessagePresentation::NativeAnsi(StyledText {
                text: "docs".into(),
                runs: vec![StyleRun {
                    start_byte: 0,
                    end_byte: 4,
                    foreground: Some(AnsiColor::Blue),
                    background: None,
                    modifiers: StyleModifiers {
                        underline: true,
                        ..StyleModifiers::default()
                    },
                }],
            }),
            attachments: Vec::new(),
            timestamp_ms: None,
        }));
        let mut terminal = Terminal::new(TestBackend::new(40, 12)).unwrap();
        let mut cache = HistoryRenderCache::default();

        super::draw_terminal(&mut terminal, &app, &Default::default(), &mut cache).unwrap();
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .any(|cell| cell.symbol().contains("https://example.test"))
        );

        app.turns[0].final_answer.as_mut().unwrap().text =
            "[docs](mailto:user@example.test)".into();
        cache.invalidate();
        super::draw_terminal(&mut terminal, &app, &Default::default(), &mut cache).unwrap();

        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .all(|cell| !cell.symbol().contains("\u{1b}]8;;"))
        );
    }

    #[test]
    fn stale_link_reset_skips_wide_glyph_continuation_cells() {
        let buffer = ratatui::buffer::Buffer::with_lines(["界X"]);

        let cells = plain_cells(
            &buffer,
            &[HyperlinkArea {
                x: 0,
                y: 0,
                width: 2,
            }],
        );

        assert_eq!(
            cells
                .iter()
                .map(|(x, y, cell)| (*x, *y, cell.symbol()))
                .collect::<Vec<_>>(),
            vec![(0, 0, "界")]
        );
        assert_eq!(buffer[(2, 0)].symbol(), "X");

        let continuation_only = plain_cells(
            &buffer,
            &[HyperlinkArea {
                x: 1,
                y: 0,
                width: 1,
            }],
        );
        assert!(
            continuation_only.is_empty(),
            "a stale area starting inside a wide glyph must not erase it"
        );
    }

    #[test]
    fn hyperlink_patches_allow_only_local_file_urls() {
        let row = |url: &str| VisualRow {
            spans: vec![VisualSpan {
                text: "/Users/example/SKILL.md".into(),
                style: CellStyle::default(),
                hyperlink: Some(url.into()),
            }],
            fill: None,
        };
        let area = Rect::new(0, 0, 80, 1);

        let allowed = hyperlink_patches(&[row("file:///Users/example/SKILL.md")], area);
        assert_eq!(allowed.len(), 1);
        assert_eq!(allowed[0].url, "file:///Users/example/SKILL.md");

        for rejected in [
            "file://host/share.md",
            "file:////host/share.md",
            "file:///tmp/unsafe\u{1b}]8;;injected",
        ] {
            assert!(
                hyperlink_patches(&[row(rejected)], area).is_empty(),
                "destination must be rejected: {rejected:?}"
            );
        }
    }

    #[test]
    fn crossterm_bytes_close_osc_before_restoring_the_composer_cursor() {
        let mut app = AppState::default();
        app.apply(AppEvent::NativeUser(Message::text("u1", "prompt", None)));
        app.apply(AppEvent::NativeFinal(Message::final_text(
            "a1",
            "[OpenAI](https://openai.com)",
            None,
        )));
        let writer = RecordingWriter::default();
        let backend = CrosstermBackend::new(writer.clone());
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Fixed(Rect::new(0, 0, 50, 14)),
            },
        )
        .unwrap();
        let mut cache = HistoryRenderCache::default();

        super::draw_terminal(&mut terminal, &app, &Default::default(), &mut cache).unwrap();

        let bytes = writer.bytes();
        let osc = b"\x1b]8;;https://openai.com\x07OpenAI\x1b]8;;\x07";
        let osc_end = bytes
            .windows(osc.len())
            .position(|window| window == osc)
            .expect("backend should emit the exact balanced OSC 8 sequence")
            + osc.len();
        assert!(
            bytes[osc_end..]
                .windows(b"\x1b[12;2H".len())
                .any(|window| window == b"\x1b[12;2H"),
            "cursor restoration must be emitted after the OSC close"
        );
    }
}
