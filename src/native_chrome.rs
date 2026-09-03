//! Shared recognition of the native Codex and Claude terminal chrome.
//!
//! The footer, the rules and the composer marker are read by the ANSI capture
//! path, by the composer classifier and by the status line. One implementation
//! keeps them from drifting: a footer rule that only knows some model names
//! silently disables capture in one place and blocks the composer in another,
//! and it had to be corrected in every copy at once.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LineRange {
    pub start: usize,
    pub end: usize,
}

pub(crate) fn line_ranges(text: &str) -> Vec<LineRange> {
    let mut ranges = Vec::new();
    let mut start = 0;
    for (index, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            ranges.push(LineRange { start, end: index });
            start = index + 1;
        }
    }
    if start < text.len() || text.ends_with('\n') {
        ranges.push(LineRange {
            start,
            end: text.len(),
        });
    }
    ranges
}

pub(crate) fn line_text(text: &str, range: LineRange) -> &str {
    &text[range.start..range.end]
}

/// The bullets Claude prints in the first column of a line it authored.
///
/// The shipping build moved from `\u{23fa}` to `\u{25cf}`; a build older than
/// that move still prints the first. Both are listed so a pane on either one is
/// read the same way, and so the composer classifier and the capture path agree
/// on what "the agent wrote this line" means.
pub(crate) const CLAUDE_ROLE_PREFIXES: &[&str] = &["\u{23fa} ", "\u{25cf} "];

pub(crate) fn starts_with_any(line: &str, prefixes: &[&str]) -> bool {
    prefixes.iter().any(|prefix| line.starts_with(prefix))
}

pub(crate) fn is_pure_separator(line: &str, minimum_width: usize) -> bool {
    line.chars().count() >= minimum_width && line.chars().all(|character| character == '─')
}

pub(crate) fn valid_elapsed_label(label: &str) -> bool {
    let mut parts = label.split_ascii_whitespace().peekable();
    if parts.peek().is_none() {
        return false;
    }
    parts.all(|part| {
        let Some(unit) = part.chars().last() else {
            return false;
        };
        matches!(unit, 'h' | 'm' | 's')
            && part.len() > unit.len_utf8()
            && part[..part.len() - unit.len_utf8()]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
    })
}

/// A model chip is any short label an agent could print for its own model.
///
/// Matching known names instead breaks on every rename: a Claude pane running
/// Sonnet, or a Codex pane on a model not spelled `gpt-*`, looked to every
/// caller like a pane with no footer at all.
pub(crate) fn valid_model_label(model: &str) -> bool {
    !model.is_empty()
        && model.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || character.is_ascii_whitespace()
                || matches!(character, '-' | '_' | '.')
        })
}

pub(crate) fn footer_fields(line: &str) -> Vec<&str> {
    line.split('·')
        .map(str::trim)
        .filter(|field| !field.is_empty())
        .collect()
}

fn is_workdir_field(field: &str) -> bool {
    field == "~" || field.starts_with("~/") || field.starts_with('/')
}

fn is_percentage(value: &str) -> bool {
    value
        .strip_suffix('%')
        .and_then(|digits| digits.parse::<u8>().ok())
        .is_some_and(|percentage| percentage <= 100)
}

fn is_context_usage_field(field: &str) -> bool {
    let Some(value) = field.strip_prefix("Context ") else {
        return false;
    };
    value == "…" || value.strip_suffix(" used").is_some_and(is_percentage)
}

fn is_weekly_usage_field(field: &str) -> bool {
    let Some(value) = field.strip_prefix("weekly ") else {
        return false;
    };
    value == "…" || is_percentage(value) || value.strip_suffix(" left").is_some_and(is_percentage)
}

fn has_responsive_usage_fields(fields: &[&str]) -> bool {
    fields.iter().any(|field| is_context_usage_field(field))
        && (fields.iter().any(|field| is_weekly_usage_field(field)) || fields.contains(&"…"))
}

/// Returns the model chip of a native footer line, if the line is one.
///
/// A footer is recognised by its shape: a model chip first, followed by either
/// a working directory or the structured usage fields Codex retains in its
/// responsive narrow layout.
pub(crate) fn footer_model(line: &str) -> Option<&str> {
    let fields = footer_fields(line);
    let [model, rest @ ..] = fields.as_slice() else {
        return None;
    };
    if !valid_model_label(model) {
        return None;
    }
    (rest.iter().any(|field| is_workdir_field(field)) || has_responsive_usage_fields(rest))
        .then_some(*model)
}

pub(crate) fn is_known_footer(line: &str) -> bool {
    footer_model(line).is_some()
}

/// A narrow Codex footer can wrap the final `left · …` fragment onto a second
/// terminal row. It is accepted only after a fully recognised footer start.
pub(crate) fn is_known_footer_continuation(line: &str) -> bool {
    matches!(line.trim(), "left · …" | "left ·…")
}

/// Codex replaces its ordinary model footer while a turn is active and the
/// native composer contains a queued message. This exact chrome is sufficient
/// to anchor that composer even though no model or working directory remains.
pub(crate) fn is_codex_queue_footer(line: &str) -> bool {
    let fields = line.split_ascii_whitespace().collect::<Vec<_>>();
    matches!(
        fields.as_slice(),
        ["tab", "to", "queue", "message", percentage, "context", "left"]
            if is_percentage(percentage)
    )
}

/// Byte offset of the composer content on a prompt line, if the line is one.
///
/// The separator after the marker is any whitespace, not just an ASCII space:
/// Claude renders an *empty* composer as `❯` followed by U+00A0, so requiring
/// `"❯ "` failed to recognise the composer in the one state where typing into
/// it is safe, and the overlay refused all input.
pub(crate) fn composer_content_start(line: &str, marker: char) -> Option<usize> {
    let rest = line.strip_prefix(marker)?;
    match rest.chars().next() {
        None => Some(marker.len_utf8()),
        Some(character) if character.is_whitespace() => {
            Some(marker.len_utf8() + character.len_utf8())
        }
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        composer_content_start, footer_model, is_codex_queue_footer, is_known_footer,
        is_known_footer_continuation, valid_model_label,
    };

    #[test]
    fn footers_are_recognised_for_any_model_chip() {
        for (line, model) in [
            ("Claude Opus · /repo", "Claude Opus"),
            ("Sonnet 4.5 · /repo", "Sonnet 4.5"),
            ("Haiku 4.5 · ~/projects/demo", "Haiku 4.5"),
            (
                "  gpt-5.6-sol xhigh · ~/projects/own_projects/coachtm · weekly 10% left",
                "gpt-5.6-sol xhigh",
            ),
            ("GPT-5.3-Codex-Spark · /repo", "GPT-5.3-Codex-Spark"),
            ("o4-mini · /repo · 12% left", "o4-mini"),
            (
                "gpt-5.6-sol high · agent-dashboard · Context 21% used · weekly 62%",
                "gpt-5.6-sol high",
            ),
            (
                "gpt-5.6-sol high · Context … · weekly …",
                "gpt-5.6-sol high",
            ),
            (
                "gpt-5.6-sol high · agent-dashboard · Context 43% used · …",
                "gpt-5.6-sol high",
            ),
        ] {
            assert_eq!(footer_model(line), Some(model), "footer: {line:?}");
        }
    }

    #[test]
    fn ordinary_output_lines_are_not_footers() {
        for line in [
            "",
            "just prose",
            "prose · more prose",
            "· /repo",
            "⚠ Heads up, you have less than 10% of your weekly limit left.",
            "emoji 🚀 model · /repo",
            "model · Context 43% used",
            "model · weekly 62% left",
            "model · Context 101% used · …",
        ] {
            assert!(!is_known_footer(line), "must not be a footer: {line:?}");
        }
    }

    #[test]
    fn model_labels_reject_decorated_text() {
        assert!(valid_model_label("Sonnet 4.5"));
        assert!(!valid_model_label(""));
        assert!(!valid_model_label("❯ prompt"));
    }

    #[test]
    fn composer_markers_accept_any_whitespace_separator() {
        assert_eq!(
            composer_content_start("❯\u{a0}", '❯'),
            Some("❯\u{a0}".len())
        );
        assert_eq!(composer_content_start("❯ text", '❯'), Some("❯ ".len()));
        assert_eq!(composer_content_start("❯", '❯'), Some("❯".len()));
        assert_eq!(composer_content_start("❯text", '❯'), None);
        assert_eq!(composer_content_start("› run", '›'), Some("› ".len()));
        assert_eq!(composer_content_start("plain", '›'), None);
    }

    #[test]
    fn only_the_observed_narrow_footer_tail_is_a_continuation() {
        assert!(is_known_footer_continuation("  left · …"));
        assert!(is_known_footer_continuation("left ·…"));
        assert!(!is_known_footer_continuation("left"));
        assert!(!is_known_footer_continuation("ordinary prose"));
        assert!(!is_known_footer_continuation("weekly 62% left"));
    }

    #[test]
    fn codex_queue_footer_requires_the_exact_working_composer_chrome() {
        assert!(is_codex_queue_footer(
            "  tab to queue message                  55% context left"
        ));
        assert!(is_codex_queue_footer(
            "tab to queue message 0% context left"
        ));
        assert!(!is_codex_queue_footer("tab to queue message context left"));
        assert!(!is_codex_queue_footer(
            "tab to queue message 101% context left"
        ));
        assert!(!is_codex_queue_footer(
            "tab to send message 55% context left"
        ));
    }
}
