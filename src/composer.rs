use crate::agent::AgentKind;
use crate::native_chrome::{
    CLAUDE_ROLE_PREFIXES, LineRange, composer_content_start, is_codex_queue_footer,
    is_known_footer, is_known_footer_continuation, is_pure_separator, line_ranges, line_text,
    starts_with_any,
};
use crate::style::{AnsiColor, StyleRun, StyledText, validate_styled_text};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeComposerState {
    Clear,
    OwnedAttachments(usize),
    Occupied,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComposerAccess {
    Ready,
    Occupied,
    Unknown,
}

impl NativeComposerState {
    pub fn access(self, expected_attachments: usize) -> ComposerAccess {
        match self {
            Self::Clear if expected_attachments == 0 => ComposerAccess::Ready,
            Self::OwnedAttachments(actual) if actual == expected_attachments => {
                ComposerAccess::Ready
            }
            Self::Unknown => ComposerAccess::Unknown,
            Self::Clear | Self::OwnedAttachments(_) | Self::Occupied => ComposerAccess::Occupied,
        }
    }
}

pub fn classify_native_composer(kind: AgentKind, surface: &StyledText) -> NativeComposerState {
    if validate_styled_text(surface).is_err() {
        return NativeComposerState::Unknown;
    }
    let lines = line_ranges(&surface.text);
    let Some(chunks) = (match kind {
        AgentKind::Codex => codex_content(&surface.text, &lines),
        AgentKind::Claude => claude_content(&surface.text, &lines),
    }) else {
        return NativeComposerState::Unknown;
    };

    classify_content(surface, &chunks)
}

fn codex_content(text: &str, lines: &[LineRange]) -> Option<Vec<LineRange>> {
    let footer = lines.iter().rposition(|line| {
        let line = line_text(text, *line);
        is_known_footer(line) || is_codex_queue_footer(line)
    })?;
    if lines[footer + 1..].iter().any(|line| {
        let line = line_text(text, *line);
        !line.trim().is_empty() && !is_known_footer_continuation(line)
    }) {
        return None;
    }

    // Nothing is required above the composer. Codex prints notices there —
    // a weekly-limit warning, for one — and demanding a rule or an elapsed
    // label made every pane carrying one unverifiable, so the overlay refused
    // all input. The composer is already pinned by the footer below it and by
    // the continuation shape between the two.
    let prompt = (0..footer)
        .rev()
        .find(|index| composer_content_start(line_text(text, lines[*index]), '›').is_some())?;

    let first = lines[prompt];
    let prefix_len = composer_content_start(line_text(text, first), '›')?;
    let mut chunks = vec![LineRange {
        start: first.start + prefix_len,
        end: first.end,
    }];
    for line in &lines[prompt + 1..footer] {
        let value = line_text(text, *line);
        if value.is_empty() {
            chunks.push(*line);
        } else {
            let content = value.strip_prefix("  ")?;
            chunks.push(LineRange {
                start: line.end - content.len(),
                end: line.end,
            });
        }
    }
    Some(chunks)
}

/// Claude's composer is bounded by a pair of rule lines, and what follows them
/// is a mode hint rather than a footer.
///
/// The shipping build prints `⏵⏵ accept edits on (shift+tab to cycle) · esc to
/// interrupt · ← for agents` there — no model, no working directory. Demanding
/// a `model · cwd` footer therefore never matched, and the overlay treated
/// every Claude pane as unverifiable. Anchor on the rules instead and require
/// only that nothing agent-authored follows them.
/// Whether Claude authored a line, as opposed to drawing chrome around it.
///
/// Claude opens each line it writes with a bullet in the first column, and
/// indents the chrome below the composer. Counting the chrome lines instead
/// assumed the mode hint was the only one; a custom `statusLine` prints its own
/// line above that hint, and every pane configured with one became
/// unverifiable, so the overlay refused all typing.
fn is_agent_authored(line: &str) -> bool {
    starts_with_any(line, CLAUDE_ROLE_PREFIXES)
}

fn claude_content(text: &str, lines: &[LineRange]) -> Option<Vec<LineRange>> {
    let close = lines
        .iter()
        .rposition(|line| is_pure_separator(line_text(text, *line), 16))?;
    if lines[close + 1..]
        .iter()
        .any(|line| is_agent_authored(line_text(text, *line)))
    {
        return None;
    }

    let open = (0..close)
        .rev()
        .find(|index| is_pure_separator(line_text(text, lines[*index]), 16))?;
    let prompt = (open + 1..close)
        .find(|index| composer_content_start(line_text(text, lines[*index]), '❯').is_some())?;
    if lines[open + 1..prompt]
        .iter()
        .any(|line| !line_text(text, *line).trim().is_empty())
    {
        return None;
    }

    let first = lines[prompt];
    let prefix_len = composer_content_start(line_text(text, first), '❯')?;
    let mut chunks = vec![LineRange {
        start: first.start + prefix_len,
        end: first.end,
    }];
    chunks.extend(lines[prompt + 1..close].iter().copied());
    Some(chunks)
}

/// The text a native composer is holding, if it is holding any.
///
/// Only text the user typed is returned: a placeholder classifies as clear, and
/// attachment markers classify as owned attachments, so neither reaches here.
pub fn native_composer_text(kind: AgentKind, surface: &StyledText) -> Option<String> {
    native_composer_parts(kind, surface)
        .filter(|parts| parts.markers.is_empty())
        .map(|parts| parts.text)
}

/// What a native composer is holding, split into the images it carries and the
/// text beside them.
///
/// An image cannot be carried anywhere: the marker is a reference to a buffer
/// the overlay has no access to. The text beside it can be lifted out, but only
/// when every marker precedes it — an image pasted into the middle of a phrase
/// leaves no way to tell which characters belong to which side.
/// How many images the native composer is holding, when that can be told.
///
/// `None` means the pane could not be read well enough to say, and nothing
/// should be concluded from it.
pub fn native_attachment_markers(kind: AgentKind, surface: &StyledText) -> Option<Vec<usize>> {
    match classify_native_composer(kind, surface) {
        NativeComposerState::Clear => Some(Vec::new()),
        NativeComposerState::OwnedAttachments(_) | NativeComposerState::Occupied => {
            native_composer_parts(kind, surface).map(|parts| parts.markers)
        }
        NativeComposerState::Unknown => None,
    }
}

/// The raw text a native composer is showing, markers and all.
///
/// Unlike [`native_composer_parts`] this makes no sense of the content — it is
/// for looking at a composer mid-edit, when the markers and the text are not
/// yet in any tidy order.
pub fn native_composer_content(kind: AgentKind, surface: &StyledText) -> Option<String> {
    if validate_styled_text(surface).is_err() {
        return None;
    }
    let lines = line_ranges(&surface.text);
    let chunks = match kind {
        AgentKind::Codex => codex_content(&surface.text, &lines),
        AgentKind::Claude => claude_content(&surface.text, &lines),
    }?;
    Some(chunk_text(surface, &chunks))
}

pub fn native_composer_parts(kind: AgentKind, surface: &StyledText) -> Option<ComposerParts> {
    // An image on its own counts too: it has no text to lift, but the overlay
    // still has to learn about it, or it treats the pane as holding something
    // unknown and refuses every keystroke.
    let held = match classify_native_composer(kind, surface) {
        NativeComposerState::Occupied => 0,
        NativeComposerState::OwnedAttachments(count) => count,
        NativeComposerState::Clear | NativeComposerState::Unknown => return None,
    };
    let lines = line_ranges(&surface.text);
    let chunks = match kind {
        AgentKind::Codex => codex_content(&surface.text, &lines),
        AgentKind::Claude => claude_content(&surface.text, &lines),
    }?;
    let content = chunk_text(surface, &chunks);
    let (markers, text) = split_attachments(&content)?;
    if markers.is_empty() && text.trim().is_empty() {
        return None;
    }
    debug_assert!(held == 0 || held == markers.len());
    Some(ComposerParts {
        markers,
        text: text.trim().to_owned(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComposerParts {
    /// The number the pane printed for each image, in the order they appear.
    /// Agents number an image when it is pasted and keep that number for the
    /// rest of the session, so it identifies the image rather than its place.
    pub markers: Vec<usize>,
    pub text: String,
}

impl ComposerParts {
    pub fn attachments(&self) -> usize {
        self.markers.len()
    }
}

/// Counts the attachment markers that open the content and returns the rest.
/// A marker anywhere after the text means the two are interleaved.
fn split_attachments(content: &str) -> Option<(Vec<usize>, &str)> {
    let mut rest = content;
    let mut markers = Vec::new();
    loop {
        let trimmed = rest.trim_start();
        let Some(after) = trimmed.strip_prefix(ATTACHMENT_MARKER) else {
            break;
        };
        let digits = after.chars().take_while(char::is_ascii_digit).count();
        let Some(remainder) = after[digits..].strip_prefix(']') else {
            break;
        };
        let Ok(number) = after[..digits].parse() else {
            break;
        };
        markers.push(number);
        rest = remainder;
    }
    let rest = rest.trim_start();
    (!rest.contains(ATTACHMENT_MARKER)).then_some((markers, rest))
}

fn chunk_text(surface: &StyledText, chunks: &[LineRange]) -> String {
    let mut content = String::new();
    for (index, chunk) in chunks.iter().enumerate() {
        if index > 0 {
            content.push('\n');
        }
        content.push_str(&surface.text[chunk.start..chunk.end]);
    }
    content
}

fn classify_content(surface: &StyledText, chunks: &[LineRange]) -> NativeComposerState {
    let content = chunk_text(surface, chunks);
    if content.trim().is_empty() {
        return NativeComposerState::Clear;
    }
    if let Some(count) = exact_attachment_count(&content) {
        return NativeComposerState::OwnedAttachments(count);
    }
    if chunks.iter().all(|chunk| {
        surface.text[chunk.start..chunk.end]
            .char_indices()
            .filter(|(_, character)| !character.is_whitespace())
            .all(|(offset, character)| {
                placeholder_style_at(
                    &surface.runs,
                    chunk.start + offset,
                    chunk.start + offset + character.len_utf8(),
                )
            })
    }) {
        return NativeComposerState::Clear;
    }
    NativeComposerState::Occupied
}

fn placeholder_style_at(runs: &[StyleRun], start: usize, end: usize) -> bool {
    runs.iter().any(|run| {
        run.start_byte <= start
            && run.end_byte >= end
            && ((run.modifiers.dim && run.foreground.is_none())
                || matches!(
                    run.foreground,
                    Some(AnsiColor::BrightBlack | AnsiColor::Indexed(8))
                ))
    })
}

const ATTACHMENT_MARKER: &str = "[Image #";

fn exact_attachment_count(content: &str) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut index = 0;
    let mut count = 0;
    while index < bytes.len() {
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if index == bytes.len() {
            break;
        }
        let prefix = ATTACHMENT_MARKER.as_bytes();
        if !bytes[index..].starts_with(prefix) {
            return None;
        }
        index += prefix.len();
        let digits_start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == digits_start || bytes.get(index) != Some(&b']') {
            return None;
        }
        index += 1;
        if bytes
            .get(index)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        {
            return None;
        }
        count += 1;
    }
    (count > 0).then_some(count)
}
