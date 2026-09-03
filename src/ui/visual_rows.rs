use crate::app::AppState;
use crate::local_time::{format_local_timestamp, format_timestamp_at_offset};
use crate::markdown::{
    HyperlinkRange, MarkdownProjection, NonClickableLinkRange, style_markdown_with_links,
};
use crate::model::{Delivery, Message, Turn};
use crate::style::{
    AnsiColor, MessagePresentation, StyleModifiers, StyleRun, StyleRunBuilder, StyleState,
    StyledText,
};
use chrono::FixedOffset;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use unicode_width::UnicodeWidthChar;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CellStyle {
    pub foreground: Option<AnsiColor>,
    pub background: Option<AnsiColor>,
    pub modifiers: StyleModifiers,
}

impl From<&StyleRun> for CellStyle {
    fn from(run: &StyleRun) -> Self {
        Self {
            foreground: run.foreground,
            background: run.background,
            modifiers: run.modifiers,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualSpan {
    pub text: String,
    pub style: CellStyle,
    pub hyperlink: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VisualRow {
    pub spans: Vec<VisualSpan>,
    pub fill: Option<CellStyle>,
}

impl VisualRow {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            spans: vec![VisualSpan {
                text: text.into(),
                style: CellStyle::default(),
                hyperlink: None,
            }],
            fill: None,
        }
    }

    pub fn cell_width(&self) -> usize {
        self.spans
            .iter()
            .flat_map(|span| span.text.chars())
            .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
            .sum()
    }

    pub fn plain_text(&self) -> String {
        self.spans.iter().map(|span| span.text.as_str()).collect()
    }

    fn push_char(&mut self, character: char, style: CellStyle, hyperlink: Option<&str>) {
        if let Some(previous) = self.spans.last_mut()
            && previous.style == style
            && previous.hyperlink.as_deref() == hyperlink
        {
            previous.text.push(character);
        } else {
            let mut text = String::with_capacity(character.len_utf8());
            text.push(character);
            self.spans.push(VisualSpan {
                text,
                style,
                hyperlink: hyperlink.map(str::to_owned),
            });
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PromptSection {
    pub start_row: usize,
    pub content_start_row: usize,
    pub prompt_rows: usize,
    pub end_row: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoryDocument {
    pub rows: Vec<VisualRow>,
    pub prompts: Vec<PromptSection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StickyRows {
    pub source_start: usize,
    pub screen_start: usize,
    pub count: usize,
}

struct CachedTurn {
    signature: u64,
    rows: Vec<VisualRow>,
    prompt_rows: usize,
}

/// Rendered rows per turn, reused across rebuilds.
///
/// The document is rebuilt on every transcript event and every resize. Without
/// this, each rebuild re-projected the markdown and re-wrapped every turn in the
/// session, so the cost of one new answer grew with the whole history.
#[derive(Default)]
pub struct TurnRenderCache {
    width: Option<u16>,
    entries: HashMap<String, CachedTurn>,
    #[cfg(test)]
    rendered_turns: usize,
}

#[cfg(test)]
impl TurnRenderCache {
    pub fn rendered_turns(&self) -> usize {
        self.rendered_turns
    }
}

fn hash_message(message: &Message, hasher: &mut DefaultHasher) {
    message.stable_id.hash(hasher);
    message.text.hash(hasher);
    message.timestamp_ms.hash(hasher);
    for attachment in &message.attachments {
        attachment.id.hash(hasher);
        attachment.display.hash(hasher);
    }
    match &message.presentation {
        MessagePresentation::Plain => 0_u8.hash(hasher),
        MessagePresentation::MarkdownFallback => 1_u8.hash(hasher),
        MessagePresentation::NativeAnsi(styled) => {
            2_u8.hash(hasher);
            styled.text.hash(hasher);
            styled.runs.hash(hasher);
        }
    }
}

fn turn_signature(turn: &Turn) -> u64 {
    let mut hasher = DefaultHasher::new();
    hash_message(&turn.prompt, &mut hasher);
    match &turn.delivery {
        Delivery::Native => 0_u8.hash(&mut hasher),
        Delivery::Optimistic { .. } => 1_u8.hash(&mut hasher),
        Delivery::Failed { reason } => {
            2_u8.hash(&mut hasher);
            reason.hash(&mut hasher);
        }
    }
    match &turn.final_answer {
        None => 0_u8.hash(&mut hasher),
        Some(answer) => {
            1_u8.hash(&mut hasher);
            hash_message(answer, &mut hasher);
        }
    }
    hasher.finish()
}

fn turn_block(
    turn: &Turn,
    width: usize,
    format_timestamp: &mut impl FnMut(Option<u64>) -> Option<String>,
) -> (Vec<VisualRow>, usize) {
    let mut rows = Vec::new();
    rows.push(filled_timestamp_row(
        format_timestamp(turn.prompt.timestamp_ms).as_deref(),
        prompt_fill(),
        width,
        false,
        2,
    ));
    let content_start_row = rows.len();
    let mut prompt_lines = prompt_lines(&turn.prompt, &turn.delivery);
    if prompt_lines.is_empty() {
        prompt_lines.push(StyledText::default());
    }
    for line in &prompt_lines {
        push_styled_rows(&mut rows, line, prompt_fill(), prompt_text_width(width));
    }
    let prompt_rows = rows.len() - content_start_row;
    rows.push(filled_empty_row(prompt_fill()));

    if let Some(answer) = &turn.final_answer {
        if let Some(timestamp) = format_timestamp(answer.timestamp_ms) {
            rows.push(filled_timestamp_row(
                Some(timestamp.as_str()),
                None,
                width,
                false,
                2,
            ));
        }
        for line in &answer_lines(answer) {
            push_answer_rows(&mut rows, line, width);
        }
    }
    rows.push(empty_row());
    (rows, prompt_rows)
}

impl HistoryDocument {
    pub fn from_app(app: &AppState, width: u16) -> Self {
        Self::from_app_with_timestamp_formatter(app, width, format_local_timestamp)
    }

    pub fn from_app_cached(app: &AppState, width: u16, cache: &mut TurnRenderCache) -> Self {
        if cache.width != Some(width) {
            cache.entries.clear();
            cache.width = Some(width);
        }
        let cells = usize::from(width.max(1));
        let mut format_timestamp = format_local_timestamp;
        let mut document = Self::default();
        let mut live = HashSet::with_capacity(app.turns.len());
        for turn in &app.turns {
            let signature = turn_signature(turn);
            let key = turn.prompt.stable_id.as_str();
            if cache
                .entries
                .get(key)
                .is_none_or(|cached| cached.signature != signature)
            {
                let (rows, prompt_rows) = turn_block(turn, cells, &mut format_timestamp);
                cache.entries.insert(
                    key.to_owned(),
                    CachedTurn {
                        signature,
                        rows,
                        prompt_rows,
                    },
                );
                #[cfg(test)]
                {
                    cache.rendered_turns += 1;
                }
            }
            let cached = &cache.entries[key];
            let start_row = document.rows.len();
            document.rows.extend(cached.rows.iter().cloned());
            document.prompts.push(PromptSection {
                start_row,
                content_start_row: start_row + 1,
                prompt_rows: cached.prompt_rows,
                end_row: document.rows.len(),
            });
            live.insert(key.to_owned());
        }
        cache.entries.retain(|key, _| live.contains(key));
        document
    }

    pub fn from_app_at_offset(app: &AppState, width: u16, offset: FixedOffset) -> Self {
        Self::from_app_with_timestamp_formatter(app, width, |timestamp_ms| {
            format_timestamp_at_offset(timestamp_ms, offset)
        })
    }

    fn from_app_with_timestamp_formatter(
        app: &AppState,
        width: u16,
        mut format_timestamp: impl FnMut(Option<u64>) -> Option<String>,
    ) -> Self {
        let mut document = Self::default();
        let width = usize::from(width.max(1));
        for turn in &app.turns {
            let start_row = document.rows.len();
            let (rows, prompt_rows) = turn_block(turn, width, &mut format_timestamp);
            document.rows.extend(rows);
            document.prompts.push(PromptSection {
                start_row,
                content_start_row: start_row + 1,
                prompt_rows,
                end_row: document.rows.len(),
            });
        }
        document
    }

    pub fn viewport(&self, height: usize, scroll_from_bottom: usize) -> Vec<VisualRow> {
        let visible_height = height.min(self.rows.len());
        if visible_height == 0 {
            return Vec::new();
        }
        let maximum_offset = self.rows.len().saturating_sub(visible_height);
        let offset = scroll_from_bottom.min(maximum_offset);
        let top = maximum_offset.saturating_sub(offset);
        let sticky = sticky_overlay(&self.prompts, top, visible_height);
        let mut visible = Vec::with_capacity(visible_height);
        if let Some(sticky) = sticky {
            visible.extend(
                self.rows[sticky.source_start..sticky.source_start + sticky.count]
                    .iter()
                    .cloned(),
            );
            visible.extend(
                self.rows[top + sticky.count..top + visible_height]
                    .iter()
                    .cloned(),
            );
        } else {
            visible.extend(self.rows[top..top + visible_height].iter().cloned());
        }
        visible
    }
}

pub fn sticky_overlay(sections: &[PromptSection], top: usize, height: usize) -> Option<StickyRows> {
    let section_index = sections
        .iter()
        .rposition(|section| section.content_start_row < top && top < section.end_row)?;
    let section = sections[section_index];
    let content_count = 2.min(section.prompt_rows).min(height.saturating_sub(1));
    if content_count == 0 {
        return None;
    }
    let include_padding = height >= content_count + 2;
    let desired_count = content_count + usize::from(include_padding);
    let base_source_start = if include_padding {
        section.start_row
    } else {
        section.content_start_row
    };
    let mut count = desired_count;
    if let Some(next) = sections.get(section_index + 1) {
        count = count.min(next.start_row.saturating_sub(top));
    }
    (count > 0).then_some(StickyRows {
        source_start: base_source_start + (desired_count - count),
        screen_start: 0,
        count,
    })
}

/// Plain text wrapped the way it is drawn, with the source offsets to match.
///
/// The composer used to wrap one way and place its cursor another: the text was
/// drawn with word wrapping and the cursor was computed by dividing by the
/// width. They agreed only while nothing wrapped, and diverged by the length of
/// the wrapped word as soon as anything did — taking the height of the box with
/// it. Both now come from here.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WrappedText {
    pub rows: Vec<String>,
    row_starts: Vec<usize>,
}

impl WrappedText {
    /// Row and display column of a byte offset in the source.
    pub fn cell_of(&self, byte: usize) -> (usize, usize) {
        let row = match self.row_starts.binary_search(&byte) {
            Ok(row) => row,
            Err(next) => next.saturating_sub(1),
        };
        let start = self.row_starts.get(row).copied().unwrap_or(0);
        let column = self
            .rows
            .get(row)
            .map(|text| {
                text.char_indices()
                    .take_while(|(offset, _)| start + offset < byte)
                    .map(|(_, character)| UnicodeWidthChar::width(character).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);
        (row, column)
    }

    /// Where a row begins in the text it was wrapped from.
    pub fn row_start(&self, row: usize) -> usize {
        self.row_starts.get(row).copied().unwrap_or(0)
    }

    pub fn height(&self) -> usize {
        self.rows.len().max(1)
    }
}

pub fn wrap_plain(text: &str, width: usize) -> WrappedText {
    let width = width.max(1);
    let mut wrapped = WrappedText {
        rows: Vec::new(),
        row_starts: vec![0],
    };
    let mut row = String::new();
    let mut row_width = 0;
    let mut token_start = 0;

    let close_row = |wrapped: &mut WrappedText, row: &mut String, start: usize| {
        wrapped.rows.push(std::mem::take(row));
        wrapped.row_starts.push(start);
    };

    while token_start < text.len() {
        let rest = &text[token_start..];
        let token_end = rest
            .char_indices()
            .find_map(|(index, character)| (character == '\n').then_some(token_start + index))
            .unwrap_or(text.len());
        for (offset, word) in WordBoundaries::new(&text[token_start..token_end]) {
            let word_width = cell_width(word);
            if word_width <= width && row_width > 0 && row_width + word_width > width {
                close_row(&mut wrapped, &mut row, token_start + offset);
                row_width = 0;
            }
            for (word_offset, character) in word.char_indices() {
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if character_width > 0 && row_width > 0 && row_width + character_width > width {
                    close_row(&mut wrapped, &mut row, token_start + offset + word_offset);
                    row_width = 0;
                }
                row.push(character);
                row_width += character_width;
            }
        }
        if token_end == text.len() {
            break;
        }
        close_row(&mut wrapped, &mut row, token_end + 1);
        row_width = 0;
        token_start = token_end + 1;
    }
    wrapped.rows.push(row);
    wrapped
}

fn cell_width(text: &str) -> usize {
    text.chars()
        .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
        .sum()
}

pub fn wrap_styled(source: &StyledText, width: usize) -> Vec<VisualRow> {
    wrap_styled_with_hyperlinks(source, &[], width)
}

fn wrap_styled_with_hyperlinks(
    source: &StyledText,
    hyperlinks: &[HyperlinkRange],
    width: usize,
) -> Vec<VisualRow> {
    let width = width.max(1);
    let runs = valid_runs(source);
    let mut rows = Vec::new();
    let mut styles = StyleCursor::new(&runs);
    let mut links = HyperlinkCursor::new(hyperlinks);
    let mut row = empty_row();
    let mut row_width = 0;
    let mut token_start = 0;
    while token_start < source.text.len() {
        let rest = &source.text[token_start..];
        let token_end = rest
            .char_indices()
            .find_map(|(index, character)| (character == '\n').then_some(token_start + index))
            .unwrap_or(source.text.len());
        let line = &source.text[token_start..token_end];
        for (offset, word) in WordBoundaries::new(line) {
            let word_width = word
                .chars()
                .map(|character| UnicodeWidthChar::width(character).unwrap_or(0))
                .sum::<usize>();
            if word_width <= width && row_width > 0 && row_width + word_width > width {
                rows.push(row);
                row = empty_row();
                row_width = 0;
            }
            for (word_offset, character) in word.char_indices() {
                let byte = token_start + offset + word_offset;
                let style = styles.style_at(byte);
                let hyperlink = links.url_at(byte);
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if character_width > 0 && row_width > 0 && row_width + character_width > width {
                    rows.push(row);
                    row = empty_row();
                    row_width = 0;
                }
                row.push_char(character, style, hyperlink);
                row_width += character_width;
            }
        }
        if token_end == source.text.len() {
            break;
        }
        rows.push(row);
        row = empty_row();
        row_width = 0;
        token_start = token_end + 1;
    }
    rows.push(row);
    rows
}

struct HyperlinkCursor<'a> {
    ranges: &'a [HyperlinkRange],
    index: usize,
}

impl<'a> HyperlinkCursor<'a> {
    fn new(ranges: &'a [HyperlinkRange]) -> Self {
        Self { ranges, index: 0 }
    }

    fn url_at(&mut self, byte: usize) -> Option<&'a str> {
        while self
            .ranges
            .get(self.index)
            .is_some_and(|range| byte >= range.end_byte)
        {
            self.index += 1;
        }
        self.ranges
            .get(self.index)
            .filter(|range| range.start_byte <= byte)
            .map(|range| range.url.as_str())
    }
}

struct WordBoundaries<'a> {
    text: &'a str,
    offset: usize,
}

impl<'a> WordBoundaries<'a> {
    fn new(text: &'a str) -> Self {
        Self { text, offset: 0 }
    }
}

impl<'a> Iterator for WordBoundaries<'a> {
    type Item = (usize, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset == self.text.len() {
            return None;
        }
        let start = self.offset;
        let whitespace = self.text[start..].chars().next()?.is_whitespace();
        let mut end = self.text.len();
        for (offset, character) in self.text[start..].char_indices().skip(1) {
            if character.is_whitespace() != whitespace {
                end = start + offset;
                break;
            }
        }
        self.offset = end;
        Some((start, &self.text[start..end]))
    }
}

struct StyleCursor<'a> {
    runs: &'a [StyleRun],
    index: usize,
}

impl<'a> StyleCursor<'a> {
    fn new(runs: &'a [StyleRun]) -> Self {
        Self { runs, index: 0 }
    }

    fn style_at(&mut self, byte: usize) -> CellStyle {
        while self
            .runs
            .get(self.index)
            .is_some_and(|run| byte >= run.end_byte)
        {
            self.index += 1;
        }
        self.runs
            .get(self.index)
            .filter(|run| run.start_byte <= byte)
            .map(CellStyle::from)
            .unwrap_or_default()
    }
}

fn valid_runs(source: &StyledText) -> Vec<StyleRun> {
    let mut previous_end = 0;
    source
        .runs
        .iter()
        .filter(|run| {
            let valid = run.start_byte < run.end_byte
                && run.end_byte <= source.text.len()
                && source.text.is_char_boundary(run.start_byte)
                && source.text.is_char_boundary(run.end_byte)
                && run.start_byte >= previous_end;
            if valid {
                previous_end = run.end_byte;
            }
            valid
        })
        .cloned()
        .collect()
}

fn prompt_lines(message: &Message, delivery: &Delivery) -> Vec<StyledText> {
    let mut lines = message
        .text
        .split('\n')
        .map(|text| StyledText {
            text: text.into(),
            runs: Vec::new(),
        })
        .collect::<Vec<_>>();
    if message.text.is_empty()
        && let Some(attachment) = message.attachments.first()
    {
        lines[0].text = format!("[Image #1] {}", attachment.display);
    }
    let skip = if message.text.is_empty() && !message.attachments.is_empty() {
        1
    } else {
        0
    };
    lines.extend(
        message
            .attachments
            .iter()
            .enumerate()
            .skip(skip)
            .map(|(index, attachment)| StyledText {
                text: format!("[Image #{}] {}", index + 1, attachment.display),
                runs: Vec::new(),
            }),
    );
    if let Delivery::Failed { reason } = delivery {
        lines.push(StyledText {
            text: format!("not sent: {reason}"),
            runs: Vec::new(),
        });
    }
    lines
}

struct AnswerLine {
    styled: StyledText,
    hyperlinks: Vec<HyperlinkRange>,
}

fn answer_lines(message: &Message) -> Vec<AnswerLine> {
    let MarkdownProjection {
        styled: markdown_styled,
        hyperlinks: markdown_hyperlinks,
        non_clickable_links,
    } = style_markdown_with_links(&message.text);
    let (source, hyperlinks) = match &message.presentation {
        MessagePresentation::NativeAnsi(styled) => {
            if styled.text == markdown_styled.text {
                (
                    suppress_link_underlines(styled, &non_clickable_links),
                    markdown_hyperlinks,
                )
            } else {
                (styled.clone(), Vec::new())
            }
        }
        MessagePresentation::MarkdownFallback => (markdown_styled, markdown_hyperlinks),
        MessagePresentation::Plain => (
            StyledText {
                text: message.text.clone(),
                runs: Vec::new(),
            },
            Vec::new(),
        ),
    };
    split_answer_lines(&source, &hyperlinks)
}

fn suppress_link_underlines(source: &StyledText, ranges: &[NonClickableLinkRange]) -> StyledText {
    let runs = valid_runs(source);
    let mut styles = StyleCursor::new(&runs);
    let mut ranges = TextRangeCursor::new(ranges);
    let mut builder = StyleRunBuilder::new();
    for (byte, _) in source.text.char_indices() {
        let style = styles.style_at(byte);
        let mut state = StyleState {
            foreground: style.foreground,
            background: style.background,
            modifiers: style.modifiers,
        };
        if ranges.contains(byte) {
            state.modifiers.underline = false;
        }
        builder.set_style(state, byte);
    }
    StyledText {
        text: source.text.clone(),
        runs: builder.finish(source.text.len()),
    }
}

struct TextRangeCursor<'a> {
    ranges: &'a [NonClickableLinkRange],
    index: usize,
}

impl<'a> TextRangeCursor<'a> {
    fn new(ranges: &'a [NonClickableLinkRange]) -> Self {
        Self { ranges, index: 0 }
    }

    fn contains(&mut self, byte: usize) -> bool {
        while self
            .ranges
            .get(self.index)
            .is_some_and(|range| byte >= range.end_byte)
        {
            self.index += 1;
        }
        self.ranges
            .get(self.index)
            .is_some_and(|range| range.start_byte <= byte)
    }
}

fn split_answer_lines(source: &StyledText, hyperlinks: &[HyperlinkRange]) -> Vec<AnswerLine> {
    let mut lines = Vec::new();
    let mut start = 0;
    for end in source
        .text
        .match_indices('\n')
        .map(|(index, _)| index)
        .chain(std::iter::once(source.text.len()))
    {
        let text = source.text[start..end].to_owned();
        let runs = source
            .runs
            .iter()
            .filter_map(|run| {
                let from = run.start_byte.max(start);
                let to = run.end_byte.min(end);
                (from < to).then(|| StyleRun {
                    start_byte: from - start,
                    end_byte: to - start,
                    foreground: run.foreground,
                    background: run.background,
                    modifiers: run.modifiers,
                })
            })
            .collect();
        let hyperlinks = hyperlinks
            .iter()
            .filter_map(|link| {
                let from = link.start_byte.max(start);
                let to = link.end_byte.min(end);
                (from < to).then(|| HyperlinkRange {
                    start_byte: from - start,
                    end_byte: to - start,
                    url: link.url.clone(),
                })
            })
            .collect();
        lines.push(AnswerLine {
            styled: StyledText { text, runs },
            hyperlinks,
        });
        start = end.saturating_add(1);
    }
    lines
}

fn push_styled_rows(
    rows: &mut Vec<VisualRow>,
    source: &StyledText,
    fill: Option<CellStyle>,
    width: usize,
) {
    for mut row in wrap_styled(source, width) {
        row.fill = fill;
        rows.push(row);
    }
}

fn push_answer_rows(rows: &mut Vec<VisualRow>, source: &AnswerLine, width: usize) {
    for mut row in wrap_styled_with_hyperlinks(&source.styled, &source.hyperlinks, width) {
        for span in &mut row.spans {
            if span.style.foreground.is_none() {
                span.style.foreground = Some(AnsiColor::BrightWhite);
            }
        }
        rows.push(row);
    }
}

/// Prompt text stops one column short of its own band.
///
/// The band still reaches both edges, but the last column belongs to the scroll
/// indicator, and a sentence running under it would be read as damaged text
/// rather than as a thumb.
fn prompt_text_width(width: usize) -> usize {
    width.saturating_sub(1).max(1)
}

fn prompt_fill() -> Option<CellStyle> {
    Some(CellStyle {
        foreground: Some(AnsiColor::Rgb(56, 58, 66)),
        background: Some(AnsiColor::Rgb(229, 229, 230)),
        modifiers: StyleModifiers::default(),
    })
}

fn empty_row() -> VisualRow {
    VisualRow {
        spans: Vec::new(),
        fill: None,
    }
}

fn filled_empty_row(fill: Option<CellStyle>) -> VisualRow {
    VisualRow {
        spans: Vec::new(),
        fill,
    }
}

fn filled_timestamp_row(
    timestamp: Option<&str>,
    fill: Option<CellStyle>,
    width: usize,
    dim: bool,
    left_padding: usize,
) -> VisualRow {
    let clipped =
        timestamp.map(|timestamp| clip_to_width(timestamp, width.saturating_sub(left_padding)));
    let spans = clipped
        .filter(|timestamp| !timestamp.is_empty())
        .map(|timestamp| {
            vec![VisualSpan {
                text: format!("{}{timestamp}", " ".repeat(left_padding)),
                style: CellStyle {
                    foreground: Some(AnsiColor::BrightBlack),
                    modifiers: StyleModifiers {
                        dim,
                        ..StyleModifiers::default()
                    },
                    ..CellStyle::default()
                },
                hyperlink: None,
            }]
        })
        .unwrap_or_default();
    VisualRow { spans, fill }
}

fn clip_to_width(text: &str, width: usize) -> String {
    let mut clipped = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if character_width > 0 && used + character_width > width {
            break;
        }
        clipped.push(character);
        used += character_width;
    }
    clipped
}

#[cfg(test)]
mod tests {
    use super::{HistoryDocument, StyledText, TurnRenderCache, VisualRow};
    use crate::app::{AppEvent, AppState};
    use crate::model::Message;

    fn conversation(turns: u64) -> AppState {
        let mut app = AppState::default();
        for index in 0..turns {
            app.apply(AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index} with a_snake_case_identifier"),
                Some(index),
            )));
            app.apply(AppEvent::NativeFinal(Message::final_text(
                format!("a{index}"),
                format!("answer {index} with **bold** text"),
                Some(index),
            )));
        }
        app
    }

    /// Wrapping may relocate characters but must never invent or lose them, and
    /// no row may overflow the viewport it was wrapped for.
    #[test]
    fn wrapping_preserves_every_character_and_never_overflows() {
        let corpus = [
            "short",
            "a much longer line that will certainly need wrapping at small widths",
            "line one\nline two\n\nline four",
            "supercalifragilisticexpialidocious_and_then_some_more_characters",
            "界面 with 宽 glyphs mixed into ascii text",
            "trailing spaces   \n   leading spaces",
            "",
        ];

        for text in corpus {
            let source = StyledText {
                text: text.to_owned(),
                runs: Vec::new(),
            };
            for width in 2..=40_usize {
                let rows = super::wrap_styled(&source, width);

                let joined = rows
                    .iter()
                    .map(VisualRow::plain_text)
                    .collect::<Vec<_>>()
                    .join("");
                assert_eq!(
                    joined,
                    text.replace('\n', ""),
                    "characters changed at width {width} for {text:?}",
                );
                for row in &rows {
                    assert!(
                        row.cell_width() <= width,
                        "row {:?} overflows width {width}",
                        row.plain_text(),
                    );
                }
            }
        }
    }

    const WRAP_CORPUS: [&str; 7] = [
        "short",
        "aaaa bbbb cccccccccc dddd",
        "line one\nline two\n\nline four",
        "supercalifragilisticexpialidocious_and_then_some_more",
        "界面 with 宽 glyphs mixed into ascii",
        "trailing spaces   \n   leading spaces",
        "",
    ];

    /// The composer wrapping and the history wrapping have to agree, or the
    /// cursor starts drifting again somewhere no test is watching.
    #[test]
    fn plain_wrapping_agrees_with_styled_wrapping() {
        for text in WRAP_CORPUS {
            for width in 2..=40_usize {
                let plain = super::wrap_plain(text, width);
                let styled = super::wrap_styled(
                    &StyledText {
                        text: text.to_owned(),
                        runs: Vec::new(),
                    },
                    width,
                );
                assert_eq!(
                    plain.rows,
                    styled.iter().map(VisualRow::plain_text).collect::<Vec<_>>(),
                    "width {width} for {text:?}",
                );
            }
        }
    }

    #[test]
    fn every_cursor_position_lands_inside_the_wrapped_rows() {
        for text in WRAP_CORPUS {
            for width in 2..=40_usize {
                let wrapped = super::wrap_plain(text, width);
                for (byte, _) in text
                    .char_indices()
                    .chain(std::iter::once((text.len(), ' ')))
                {
                    let (row, column) = wrapped.cell_of(byte);
                    assert!(row < wrapped.rows.len(), "row {row} escapes {text:?}");
                    assert!(
                        column <= super::cell_width(&wrapped.rows[row]),
                        "column {column} escapes row {:?}",
                        wrapped.rows[row],
                    );
                }
                let (row, column) = wrapped.cell_of(text.len());
                assert_eq!(row, wrapped.rows.len() - 1);
                assert_eq!(column, super::cell_width(wrapped.rows.last().unwrap()));
            }
        }
    }

    #[test]
    fn cached_rendering_matches_the_uncached_document() {
        let app = conversation(5);
        let mut cache = TurnRenderCache::default();

        for width in [20, 40, 80] {
            assert_eq!(
                HistoryDocument::from_app_cached(&app, width, &mut cache),
                HistoryDocument::from_app(&app, width),
                "width {width}",
            );
        }
    }

    #[test]
    fn appending_a_turn_only_renders_that_turn() {
        let mut app = conversation(5);
        let mut cache = TurnRenderCache::default();

        HistoryDocument::from_app_cached(&app, 40, &mut cache);
        assert_eq!(cache.rendered_turns(), 5);

        HistoryDocument::from_app_cached(&app, 40, &mut cache);
        assert_eq!(cache.rendered_turns(), 5, "unchanged turns must be reused");

        app.apply(AppEvent::NativeUser(Message::text(
            "u5",
            "prompt 5",
            Some(5),
        )));
        app.apply(AppEvent::NativeFinal(Message::final_text(
            "a5",
            "answer 5",
            Some(5),
        )));
        let grown = HistoryDocument::from_app_cached(&app, 40, &mut cache);

        assert_eq!(cache.rendered_turns(), 6, "only the new turn is rendered");
        assert_eq!(grown, HistoryDocument::from_app(&app, 40));
    }

    #[test]
    fn an_answer_arriving_re_renders_only_its_own_turn() {
        let mut app = conversation(4);
        let mut cache = TurnRenderCache::default();
        HistoryDocument::from_app_cached(&app, 40, &mut cache);
        assert_eq!(cache.rendered_turns(), 4);

        app.turns[1].final_answer = Some(Message::final_text("a1", "revised answer", Some(9)));
        let updated = HistoryDocument::from_app_cached(&app, 40, &mut cache);

        assert_eq!(cache.rendered_turns(), 5);
        assert_eq!(updated, HistoryDocument::from_app(&app, 40));
    }

    #[test]
    fn dropped_turns_do_not_accumulate_in_the_cache() {
        let mut app = conversation(6);
        let mut cache = TurnRenderCache::default();
        HistoryDocument::from_app_cached(&app, 40, &mut cache);

        app.turns.truncate(2);
        HistoryDocument::from_app_cached(&app, 40, &mut cache);

        assert_eq!(cache.entries.len(), 2);
    }
}
