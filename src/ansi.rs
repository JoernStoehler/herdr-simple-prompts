use crate::agent::AgentKind;
use crate::native_chrome::{
    CLAUDE_ROLE_PREFIXES, LineRange, composer_content_start, is_known_footer,
    is_known_footer_continuation, is_pure_separator, line_ranges, line_text, starts_with_any,
    valid_elapsed_label,
};
use crate::style::{AnsiColor, StyleRun, StyleRunBuilder, StyleState, StyledText};
use unicode_width::UnicodeWidthChar;

const TAB_WIDTH: usize = 8;

struct NativeChrome {
    role_prefixes: &'static [&'static str],
    continuation_prefixes: &'static [&'static str],
    separator_min_width: usize,
    trailing_boundary_prefixes: &'static [&'static str],
    composer_marker: char,
    trailing: TrailingRule,
}

/// What an agent prints below its composer.
#[derive(Clone, Copy)]
enum TrailingRule {
    /// Codex ends with a real `model · cwd` footer.
    KnownFooter,
    /// Claude ends with a closing rule and chrome that names neither the model
    /// nor the working directory, so authorship is all there is to check.
    /// Counting the chrome lines instead assumed the mode hint was the only
    /// one, and a custom `statusLine` adds a line of its own above it.
    AgentFree,
}

const CODEX_CHROME: NativeChrome = NativeChrome {
    role_prefixes: &["• "],
    continuation_prefixes: &["  "],
    separator_min_width: 8,
    trailing_boundary_prefixes: &["─ Worked for "],
    composer_marker: '›',
    trailing: TrailingRule::KnownFooter,
};

const CLAUDE_CHROME: NativeChrome = NativeChrome {
    role_prefixes: CLAUDE_ROLE_PREFIXES,
    continuation_prefixes: &["  "],
    separator_min_width: 16,
    trailing_boundary_prefixes: &[],
    composer_marker: '❯',
    trailing: TrailingRule::AgentFree,
};

pub fn sanitize_ansi(input: &str) -> StyledText {
    let bytes = input.as_bytes();
    let mut cells: Vec<(char, StyleState)> = Vec::with_capacity(input.len());
    let mut column = 0;
    let mut style = StyleState::default();
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'\x1b' => index = consume_escape(bytes, index, &mut style),
            b'\r' => {
                // The source is a rendered pane grid, so a carriage return
                // terminates a line here rather than overwriting one.
                cells.push(('\n', style));
                column = 0;
                index += usize::from(bytes.get(index + 1) == Some(&b'\n')) + 1;
            }
            b'\n' => {
                cells.push(('\n', style));
                column = 0;
                index += 1;
            }
            b'\t' => {
                // Dropping tabs collapsed the indentation of captured code and
                // merged the columns either side of them into one word.
                let advance = TAB_WIDTH - (column % TAB_WIDTH);
                cells.extend(std::iter::repeat_n((' ', style), advance));
                column += advance;
                index += 1;
            }
            0x00..=0x1f | 0x7f => index += 1,
            _ => {
                let character = input[index..]
                    .chars()
                    .next()
                    .expect("index is always on a UTF-8 boundary");
                if !character.is_control() {
                    cells.push((character, style));
                    column += UnicodeWidthChar::width(character).unwrap_or(0);
                }
                index += character.len_utf8();
            }
        }
    }

    let mut text = String::with_capacity(cells.len());
    let mut runs = StyleRunBuilder::new();
    for (character, style) in cells {
        runs.set_style(style, text.len());
        text.push(character);
    }
    StyledText {
        runs: runs.finish(text.len()),
        text,
    }
}

pub fn extract_native_final(
    ansi: &str,
    expected_visible: &str,
    kind: AgentKind,
) -> Option<StyledText> {
    if expected_visible.is_empty() || expected_visible.contains('\r') {
        return None;
    }
    let sanitized = sanitize_ansi(ansi);
    let lines = line_ranges(&sanitized.text);
    let chrome = match kind {
        AgentKind::Codex => &CODEX_CHROME,
        AgentKind::Claude => &CLAUDE_CHROME,
    };
    let expected_visible_lines: Vec<&str> = expected_visible.split('\n').collect();
    let strict_candidates =
        strict_final_candidates(&sanitized, &lines, &expected_visible_lines, chrome);
    let candidate = match strict_candidates.len() {
        1 => strict_candidates.into_iter().next(),
        0 => {
            let mut reflow_candidates = Vec::new();
            for trailing in 0..lines.len() {
                if !is_trailing_boundary(line_text(&sanitized.text, lines[trailing]), chrome) {
                    continue;
                }
                let composer = trailing + 1;
                if composer >= lines.len()
                    || !is_composer_line(line_text(&sanitized.text, lines[composer]), chrome)
                {
                    continue;
                }
                for first in 0..trailing {
                    if !starts_with_any(
                        line_text(&sanitized.text, lines[first]),
                        chrome.role_prefixes,
                    ) {
                        continue;
                    }
                    if let Some(runs) = reflow_candidate_runs(
                        &sanitized,
                        &lines,
                        first,
                        trailing,
                        expected_visible,
                        chrome,
                    ) {
                        reflow_candidates.push((runs, composer));
                    }
                }
            }
            (reflow_candidates.len() == 1)
                .then(|| reflow_candidates.pop().expect("one reflow candidate"))
        }
        _ => None,
    }?;
    let (runs, composer) = candidate;
    if !trailing_is_chrome(&sanitized.text, &lines[composer + 1..], chrome) {
        return None;
    }
    Some(StyledText {
        text: expected_visible.to_owned(),
        runs,
    })
}

fn strict_final_candidates(
    sanitized: &StyledText,
    lines: &[LineRange],
    expected_visible_lines: &[&str],
    chrome: &NativeChrome,
) -> Vec<(Vec<StyleRun>, usize)> {
    let mut candidates = Vec::new();

    for boundary in 0..lines.len() {
        if !is_pure_separator(
            line_text(&sanitized.text, lines[boundary]),
            chrome.separator_min_width,
        ) {
            continue;
        }
        let first = boundary + 1;
        let Some(trailing) = first.checked_add(expected_visible_lines.len()) else {
            continue;
        };
        let composer = trailing + 1;
        if composer >= lines.len()
            || !is_trailing_boundary(line_text(&sanitized.text, lines[trailing]), chrome)
            || !is_composer_line(line_text(&sanitized.text, lines[composer]), chrome)
        {
            continue;
        }

        let mut mappings = Vec::with_capacity(expected_visible_lines.len() * 2);
        let mut destination = 0;
        let mut exact = true;
        for (offset, expected_visible_line) in expected_visible_lines.iter().enumerate() {
            let range = lines[first + offset];
            let source_line = line_text(&sanitized.text, range);
            let prefixes = if offset == 0 {
                chrome.role_prefixes
            } else {
                chrome.continuation_prefixes
            };
            let prefix = if source_line.is_empty() && expected_visible_line.is_empty() {
                ""
            } else if let Some(prefix) = prefixes
                .iter()
                .copied()
                .find(|prefix| source_line.starts_with(prefix))
            {
                prefix
            } else {
                exact = false;
                break;
            };
            let content_start = range.start + prefix.len();
            if &sanitized.text[content_start..range.end] != *expected_visible_line {
                exact = false;
                break;
            }
            mappings.push((content_start, range.end, destination));
            destination += expected_visible_line.len();
            if offset + 1 < expected_visible_lines.len() {
                let Some(newline_end) = range.end.checked_add(1) else {
                    exact = false;
                    break;
                };
                if sanitized.text.as_bytes().get(range.end) != Some(&b'\n') {
                    exact = false;
                    break;
                }
                mappings.push((range.end, newline_end, destination));
                destination += 1;
            }
        }
        if exact {
            candidates.push((slice_mapped_runs(&sanitized.runs, &mappings), composer));
        }
    }

    candidates
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScalarRange {
    character: char,
    start: usize,
    end: usize,
}

fn non_whitespace_scalars(text: &str, start: usize, end: usize) -> Vec<ScalarRange> {
    text[start..end]
        .char_indices()
        .filter_map(|(offset, character)| {
            (!character.is_whitespace()).then_some(ScalarRange {
                character,
                start: start + offset,
                end: start + offset + character.len_utf8(),
            })
        })
        .collect()
}

fn reflow_candidate_runs(
    sanitized: &StyledText,
    lines: &[LineRange],
    first: usize,
    trailing: usize,
    expected_visible: &str,
    chrome: &NativeChrome,
) -> Option<Vec<StyleRun>> {
    let mut source = Vec::new();
    for (line_index, range) in lines.iter().copied().enumerate().take(trailing).skip(first) {
        let line = line_text(&sanitized.text, range);
        let prefixes = if line_index == first {
            chrome.role_prefixes
        } else {
            chrome.continuation_prefixes
        };
        let content_start = if line.is_empty() {
            range.start
        } else {
            let prefix = prefixes.iter().find(|prefix| line.starts_with(**prefix))?;
            range.start + prefix.len()
        };
        source.extend(non_whitespace_scalars(
            &sanitized.text,
            content_start,
            range.end,
        ));
    }

    let destination = non_whitespace_scalars(expected_visible, 0, expected_visible.len());
    if destination.is_empty()
        || source.len() != destination.len()
        || source
            .iter()
            .zip(&destination)
            .any(|(left, right)| left.character != right.character)
    {
        return None;
    }

    let mappings: Vec<_> = source
        .iter()
        .zip(destination)
        .map(|(source, destination)| (source.start, source.end, destination.start))
        .collect();
    Some(slice_mapped_runs(&sanitized.runs, &mappings))
}

fn is_trailing_boundary(line: &str, chrome: &NativeChrome) -> bool {
    is_pure_separator(line, chrome.separator_min_width)
        || chrome
            .trailing_boundary_prefixes
            .iter()
            .any(|prefix| matches_decorated_boundary(line, prefix, chrome.separator_min_width))
}

fn matches_decorated_boundary(line: &str, prefix: &str, minimum_width: usize) -> bool {
    if line.chars().count() < minimum_width || !line.starts_with(prefix) {
        return false;
    }
    let remainder = &line[prefix.len()..];
    let Some((label, suffix)) = remainder.rsplit_once(" ") else {
        return false;
    };
    valid_elapsed_label(label)
        && !suffix.is_empty()
        && suffix.chars().all(|character| character == '─')
}

fn is_composer_line(line: &str, chrome: &NativeChrome) -> bool {
    composer_content_start(line, chrome.composer_marker).is_some()
}

/// Everything below the composer has to be the agent's own chrome, so that a
/// matched region is really the tail of the pane and not an older answer.
fn trailing_is_chrome(text: &str, ranges: &[LineRange], chrome: &NativeChrome) -> bool {
    let trailing = ranges
        .iter()
        .map(|range| line_text(text, *range))
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    match chrome.trailing {
        TrailingRule::KnownFooter => trailing.first().is_none_or(|line| {
            is_known_footer(line)
                && trailing[1..]
                    .iter()
                    .all(|line| is_known_footer(line) || is_known_footer_continuation(line))
        }),
        TrailingRule::AgentFree => !trailing
            .iter()
            .any(|line| starts_with_any(line, chrome.role_prefixes)),
    }
}

fn slice_mapped_runs(runs: &[StyleRun], mappings: &[(usize, usize, usize)]) -> Vec<StyleRun> {
    let mut sliced: Vec<StyleRun> = Vec::new();
    for &(source_start, source_end, destination_start) in mappings {
        for run in runs {
            let start = run.start_byte.max(source_start);
            let end = run.end_byte.min(source_end);
            if start >= end {
                continue;
            }
            let mapped = StyleRun {
                start_byte: destination_start + start - source_start,
                end_byte: destination_start + end - source_start,
                foreground: run.foreground,
                background: run.background,
                modifiers: run.modifiers,
            };
            if let Some(previous) = sliced.last_mut()
                && previous.end_byte == mapped.start_byte
                && previous.foreground == mapped.foreground
                && previous.background == mapped.background
                && previous.modifiers == mapped.modifiers
            {
                previous.end_byte = mapped.end_byte;
            } else {
                sliced.push(mapped);
            }
        }
    }
    sliced
}

fn consume_escape(bytes: &[u8], start: usize, style: &mut StyleState) -> usize {
    let Some(&kind) = bytes.get(start + 1) else {
        return bytes.len();
    };
    match kind {
        b'[' => consume_csi(bytes, start, style),
        b']' => consume_osc(bytes, start),
        b'P' | b'_' | b'^' => consume_st_string(bytes, start + 2),
        0x20..=0x2f => consume_escape_with_intermediate(bytes, start + 2),
        _ if kind.is_ascii() => (start + 2).min(bytes.len()),
        _ => start + 1,
    }
}

fn consume_csi(bytes: &[u8], start: usize, style: &mut StyleState) -> usize {
    let mut index = start + 2;
    while let Some(&byte) = bytes.get(index) {
        if (0x40..=0x7e).contains(&byte) {
            if byte == b'm' {
                apply_sgr(&bytes[start + 2..index], style);
            }
            return index + 1;
        }
        index += 1;
    }
    bytes.len()
}

fn consume_osc(bytes: &[u8], start: usize) -> usize {
    let mut index = start + 2;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'\x07' {
            return index + 1;
        }
        if byte == b'\x1b' && bytes.get(index + 1) == Some(&b'\\') {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

fn consume_st_string(bytes: &[u8], mut index: usize) -> usize {
    while let Some(&byte) = bytes.get(index) {
        if byte == b'\x1b' && bytes.get(index + 1) == Some(&b'\\') {
            return index + 2;
        }
        index += 1;
    }
    bytes.len()
}

fn consume_escape_with_intermediate(bytes: &[u8], mut index: usize) -> usize {
    while matches!(bytes.get(index), Some(0x20..=0x2f)) {
        index += 1;
    }
    if bytes.get(index).is_some() {
        index + 1
    } else {
        bytes.len()
    }
}

fn apply_sgr(parameters: &[u8], style: &mut StyleState) {
    let Some(parameters) = parse_parameters(parameters) else {
        return;
    };
    let mut next = *style;
    let mut index = 0;
    while index < parameters.len() {
        match parameters[index] {
            0 => next = StyleState::default(),
            1 => next.modifiers.bold = true,
            2 => next.modifiers.dim = true,
            3 => next.modifiers.italic = true,
            4 => next.modifiers.underline = true,
            7 => next.modifiers.reverse = true,
            22 => {
                next.modifiers.bold = false;
                next.modifiers.dim = false;
            }
            23 => next.modifiers.italic = false,
            24 => next.modifiers.underline = false,
            27 => next.modifiers.reverse = false,
            30..=37 | 90..=97 => next.foreground = named_color(parameters[index]),
            39 => next.foreground = None,
            40..=47 | 100..=107 => next.background = named_color(parameters[index]),
            49 => next.background = None,
            38 | 48 => {
                let consumed = color_parameter_len(&parameters[index..]);
                if let Some((value, _)) = extended_color(&parameters[index..]) {
                    if parameters[index] == 38 {
                        next.foreground = Some(value);
                    } else {
                        next.background = Some(value);
                    }
                }
                index += consumed.saturating_sub(1);
            }
            // Underline colour carries the same sub-parameters as 38/48.
            // Without skipping them the colour components were re-read as
            // ordinary SGR codes and could reset the run's styling.
            58 => index += color_parameter_len(&parameters[index..]).saturating_sub(1),
            _ => {}
        }
        index += 1;
    }
    *style = next;
}

fn color_parameter_len(parameters: &[u16]) -> usize {
    match parameters.get(1) {
        Some(5) => parameters.len().min(3),
        Some(2) => parameters.len().min(5),
        _ => 1,
    }
}

fn parse_parameters(parameters: &[u8]) -> Option<Vec<u16>> {
    if parameters.is_empty() {
        return Some(vec![0]);
    }
    parameters
        .split(|byte| *byte == b';')
        .map(|part| {
            if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
                return None;
            }
            std::str::from_utf8(part).ok()?.parse().ok()
        })
        .collect()
}

fn extended_color(parameters: &[u16]) -> Option<(AnsiColor, usize)> {
    match parameters {
        [_, 5, value, ..] => u8::try_from(*value)
            .ok()
            .map(|value| (AnsiColor::Indexed(value), 3)),
        [_, 2, red, green, blue, ..] => Some((
            AnsiColor::Rgb(
                u8::try_from(*red).ok()?,
                u8::try_from(*green).ok()?,
                u8::try_from(*blue).ok()?,
            ),
            5,
        )),
        _ => None,
    }
}

fn named_color(code: u16) -> Option<AnsiColor> {
    Some(match code {
        30 | 40 => AnsiColor::Black,
        31 | 41 => AnsiColor::Red,
        32 | 42 => AnsiColor::Green,
        33 | 43 => AnsiColor::Yellow,
        34 | 44 => AnsiColor::Blue,
        35 | 45 => AnsiColor::Magenta,
        36 | 46 => AnsiColor::Cyan,
        37 | 47 => AnsiColor::White,
        90 | 100 => AnsiColor::BrightBlack,
        91 | 101 => AnsiColor::BrightRed,
        92 | 102 => AnsiColor::BrightGreen,
        93 | 103 => AnsiColor::BrightYellow,
        94 | 104 => AnsiColor::BrightBlue,
        95 | 105 => AnsiColor::BrightMagenta,
        96 | 106 => AnsiColor::BrightCyan,
        97 | 107 => AnsiColor::BrightWhite,
        _ => return None,
    })
}
