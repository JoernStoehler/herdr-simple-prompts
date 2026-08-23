use chrono::FixedOffset;
use herdr_simple_prompts::agent::AgentStatus;
use herdr_simple_prompts::app::{AppEvent, AppState};
use herdr_simple_prompts::composer::NativeComposerState;
use herdr_simple_prompts::editor::Editor;
use herdr_simple_prompts::history::{PersistedPresentation, VisibleHistoryRecord, VisibleRole};
use herdr_simple_prompts::model::Attachment;
use herdr_simple_prompts::model::Message;
use herdr_simple_prompts::paste::fingerprint;
use herdr_simple_prompts::style::StyledText;
use herdr_simple_prompts::style::{AnsiColor, MessagePresentation, StyleModifiers, StyleRun};
use herdr_simple_prompts::ui::render::{
    render_terminal_to_buffer, render_to_buffer, render_to_string,
};
use herdr_simple_prompts::ui::visual_rows::{
    CellStyle, HistoryDocument, PromptSection, StickyRows, VisualRow, sticky_overlay, wrap_styled,
};
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use std::ops::Range;
use std::time::{Duration, Instant};

fn rendered_buffer(app: &AppState, width: u16, height: u16) -> Buffer {
    render_to_buffer(app, &Editor::default(), width, height)
}

fn assert_clear_horizontal_gutters(buffer: &Buffer, width: u16, height: u16) {
    assert!(width >= 2);
    for y in 0..height {
        for x in [0, width - 1] {
            assert_clear_cell(buffer, x, y);
        }
    }
}

fn assert_clear_cell(buffer: &Buffer, x: u16, y: u16) {
    let cell = &buffer[(x, y)];
    let style = cell.style();
    assert_eq!(cell.symbol(), " ", "painted gutter at ({x}, {y})");
    assert!(
        matches!(style.fg, None | Some(Color::Reset)),
        "foreground-styled gutter at ({x}, {y}): {style:?}"
    );
    assert!(
        matches!(style.bg, None | Some(Color::Reset)),
        "background-styled gutter at ({x}, {y}): {style:?}"
    );
    assert!(
        style.add_modifier.is_empty() && style.sub_modifier.is_empty(),
        "modifier-styled gutter at ({x}, {y}): {style:?}"
    );
}

/// A prompt band has to reach the edge. The scroll indicator is allowed to sit
/// on top of it - it keeps the band's background and prompt text never runs
/// under it - so the glyph is the thumb or the track, or nothing at all.
fn assert_prompt_fill_cell(buffer: &Buffer, x: u16, y: u16) {
    let cell = &buffer[(x, y)];
    assert!(
        matches!(cell.symbol(), " " | "│" | "▐"),
        "prompt edge carries text at ({x}, {y}): {:?}",
        cell.symbol()
    );
    assert_eq!(
        cell.style().bg,
        Some(Color::Rgb(52, 53, 54)),
        "prompt background does not reach ({x}, {y})"
    );
}

#[test]
fn ordinary_view_uses_one_clear_cell_on_both_horizontal_edges() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "abcdefghijklmnop",
        None,
    )));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1", "answer", None,
    )));
    app.agent_status = AgentStatus::Working;
    app.working_since = Some(Instant::now());
    let editor = Editor::default();

    let (buffer, cursor) = render_terminal_to_buffer(&app, &editor, 16, 12);

    for y in 0..=3 {
        assert_prompt_fill_cell(&buffer, 0, y);
        assert_prompt_fill_cell(&buffer, 15, y);
    }
    for y in [4, 7, 8, 11] {
        assert_clear_cell(&buffer, 0, y);
        assert_clear_cell(&buffer, 15, y);
    }
    assert_eq!(buffer[(1, 1)].symbol(), "a");
    assert_eq!(buffer[(13, 1)].symbol(), "m");
    assert_eq!(
        buffer[(14, 1)].symbol(),
        " ",
        "the scroll column stays free of prompt text"
    );
    assert_eq!(buffer[(1, 2)].symbol(), "n");
    assert_eq!(cursor.0, 1);
}

#[test]
fn prompt_is_a_label_free_gray_block_and_answer_is_unboxed() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "check dns", None)));
    app.apply(AppEvent::NativeFinal(Message::text(
        "a1",
        "zone is pending",
        None,
    )));

    let rendered = render_to_string(&app, &Editor::default(), 50, 14);
    let document = HistoryDocument::from_app(&app, 50);

    assert!(!rendered.contains("YOU"));
    assert!(!rendered.contains("ANSWER"));
    assert_eq!(document.rows[0].plain_text(), "");
    assert_eq!(document.rows[1].plain_text(), "check dns");
    assert_eq!(document.rows[2].plain_text(), "");
    assert_eq!(document.rows[3].plain_text(), "zone is pending");
    assert_eq!(document.rows[0].fill, document.rows[1].fill);
    assert_eq!(document.rows[1].fill, document.rows[2].fill);
    assert!(document.rows[0].fill.is_some());
    assert!(document.rows[3].fill.is_none());
}

#[test]
fn unstyled_agent_answer_uses_a_bright_foreground() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "prompt", None)));
    app.apply(AppEvent::NativeFinal(Message::text("a1", "Z", None)));

    let buffer = rendered_buffer(&app, 40, 12);
    let answer = find_cell(&buffer, 40, 12, "Z");

    assert_eq!(buffer[answer].style().fg, Some(Color::White));
}

#[test]
fn timestamp_uses_the_existing_top_prompt_row_at_a_fixed_offset() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "check dns",
        Some(1_786_638_720_000),
    )));
    app.apply(AppEvent::NativeFinal(Message::text(
        "a1",
        "zone is pending",
        None,
    )));

    let document =
        HistoryDocument::from_app_at_offset(&app, 50, FixedOffset::east_opt(3 * 60 * 60).unwrap());

    assert_eq!(document.rows.len(), 5);
    assert_eq!(document.rows[0].plain_text(), "  13.08.2026 19:32");
    assert_eq!(document.rows[1].plain_text(), "check dns");
    assert_eq!(document.rows[2].plain_text(), "");
    assert_eq!(document.rows[3].plain_text(), "zone is pending");
    assert_eq!(document.rows[0].fill, document.rows[1].fill);
    assert_eq!(document.rows[1].fill, document.rows[2].fill);
    assert_eq!(
        document.rows[0].spans[0].style.foreground,
        Some(AnsiColor::BrightBlack)
    );
    assert!(!document.rows[0].spans[0].style.modifiers.dim);

    let buffer = rendered_buffer(&app, 50, 14);
    assert_prompt_fill_cell(&buffer, 0, 0);
    assert_prompt_fill_cell(&buffer, 49, 0);
    assert_eq!(buffer[(1, 0)].symbol(), " ");
    assert_eq!(buffer[(2, 0)].symbol(), " ");
    assert!(
        buffer[(3, 0)]
            .symbol()
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    );
}

#[test]
fn answer_timestamp_is_an_undimmed_gray_row_above_the_styled_body() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "show native",
        None,
    )));
    app.apply(AppEvent::NativeFinal(Message {
        stable_id: "a1".into(),
        text: "Native answer".into(),
        presentation: MessagePresentation::NativeAnsi(StyledText {
            text: "Native answer".into(),
            runs: vec![StyleRun {
                start_byte: 0,
                end_byte: "Native answer".len(),
                foreground: Some(AnsiColor::Green),
                background: None,
                modifiers: StyleModifiers {
                    bold: true,
                    ..StyleModifiers::default()
                },
            }],
        }),
        attachments: Vec::new(),
        timestamp_ms: Some(1_786_638_720_000),
    }));

    let document =
        HistoryDocument::from_app_at_offset(&app, 50, FixedOffset::east_opt(3 * 60 * 60).unwrap());

    assert_eq!(document.rows.len(), 6);
    assert_eq!(document.rows[3].plain_text(), "  13.08.2026 19:32");
    assert!(document.rows[3].fill.is_none());
    assert_eq!(
        document.rows[3].spans[0].style.foreground,
        Some(AnsiColor::BrightBlack)
    );
    assert!(!document.rows[3].spans[0].style.modifiers.dim);
    assert_eq!(document.rows[4].plain_text(), "Native answer");
    assert_eq!(
        document.rows[4].spans[0].style.foreground,
        Some(AnsiColor::Green)
    );
    assert!(document.rows[4].spans[0].style.modifiers.bold);

    let buffer = rendered_buffer(&app, 50, 14);
    assert_clear_cell(&buffer, 0, 3);
    assert_eq!(buffer[(1, 3)].symbol(), " ");
    assert_eq!(buffer[(2, 3)].symbol(), " ");
    assert!(
        buffer[(3, 3)]
            .symbol()
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_digit())
    );
}

#[test]
fn answer_timestamp_is_clipped_to_one_visual_row() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "prompt", None)));
    app.apply(AppEvent::NativeFinal(Message::text(
        "a1",
        "done",
        Some(1_786_638_720_000),
    )));

    let document =
        HistoryDocument::from_app_at_offset(&app, 8, FixedOffset::east_opt(3 * 60 * 60).unwrap());

    assert_eq!(document.rows[3].plain_text(), "  13.08.");
    assert_eq!(document.rows[3].cell_width(), 8);
    assert_eq!(document.rows[4].plain_text(), "done");
}

#[test]
fn answer_timestamp_is_omitted_without_a_valid_value() {
    for timestamp_ms in [None, Some(u64::MAX)] {
        let mut app = AppState::default();
        app.apply(AppEvent::NativeUser(Message::text("u1", "prompt", None)));
        app.apply(AppEvent::NativeFinal(Message::text(
            "a1",
            "answer",
            timestamp_ms,
        )));

        let document = HistoryDocument::from_app_at_offset(
            &app,
            50,
            FixedOffset::east_opt(3 * 60 * 60).unwrap(),
        );

        assert_eq!(document.rows.len(), 5);
        assert_eq!(document.rows[3].plain_text(), "answer");
        assert_eq!(document.rows[4].plain_text(), "");
    }
}

#[test]
fn answer_timestamp_survives_visible_history_hydration() {
    let mut app = AppState::default();
    app.hydrate_visible_history(vec![
        VisibleHistoryRecord {
            version: 2,
            role: VisibleRole::Prompt,
            stable_id: "saved-prompt".into(),
            turn_id: "saved-prompt".into(),
            order: 1,
            text: "hydrated prompt".into(),
            attachments: Vec::new(),
            timestamp_ms: None,
            text_fingerprint: fingerprint("hydrated prompt"),
            presentation: PersistedPresentation::Plain,
            rendered_text: None,
            rendered_text_fingerprint: None,
        },
        VisibleHistoryRecord {
            version: 2,
            role: VisibleRole::Final,
            stable_id: "saved-answer".into(),
            turn_id: "saved-prompt".into(),
            order: 2,
            text: "hydrated answer".into(),
            attachments: Vec::new(),
            timestamp_ms: Some(1_786_638_720_000),
            text_fingerprint: fingerprint("hydrated answer"),
            presentation: PersistedPresentation::Fallback,
            rendered_text: None,
            rendered_text_fingerprint: None,
        },
    ]);

    let document =
        HistoryDocument::from_app_at_offset(&app, 50, FixedOffset::east_opt(3 * 60 * 60).unwrap());

    assert_eq!(document.rows[3].plain_text(), "  13.08.2026 19:32");
    assert_eq!(document.rows[4].plain_text(), "hydrated answer");
}

#[test]
fn narrow_timestamp_is_clipped_to_one_gray_row_without_growing_the_prompt() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "prompt",
        Some(1_786_638_720_000),
    )));

    let document =
        HistoryDocument::from_app_at_offset(&app, 8, FixedOffset::east_opt(3 * 60 * 60).unwrap());

    assert_eq!(document.rows.len(), 4);
    assert_eq!(document.rows[0].plain_text(), "  13.08.");
    assert_eq!(document.rows[0].cell_width(), 8);
    assert_eq!(document.rows[1].plain_text(), "prompt");
    assert_eq!(document.rows[2].plain_text(), "");
    assert!(document.rows[0].fill.is_some());
}

#[test]
fn missing_or_invalid_timestamp_leaves_the_existing_top_gray_row_blank() {
    for timestamp_ms in [None, Some(u64::MAX)] {
        let mut app = AppState::default();
        app.apply(AppEvent::NativeUser(Message::text(
            "u1",
            "legacy prompt",
            timestamp_ms,
        )));

        let document = HistoryDocument::from_app_at_offset(
            &app,
            50,
            FixedOffset::east_opt(3 * 60 * 60).unwrap(),
        );

        assert_eq!(document.rows.len(), 4);
        assert_eq!(document.rows[0].plain_text(), "");
        assert_eq!(document.rows[1].plain_text(), "legacy prompt");
        assert!(document.rows[0].fill.is_some());
    }
}

#[test]
fn optimistic_and_hydrated_prompts_render_their_owned_timestamps() {
    let offset = FixedOffset::east_opt(3 * 60 * 60).unwrap();
    let mut optimistic = AppState::default();
    let mut editor = Editor::default();
    editor.replace("optimistic prompt");
    optimistic.apply(AppEvent::PromptSubmitted {
        local_id: "local-1".into(),
        submission: editor.take_editor_submission(),
        attachments: Vec::new(),
        at_ms: 1_786_638_720_000,
    });

    let optimistic_document = HistoryDocument::from_app_at_offset(&optimistic, 50, offset);

    assert_eq!(
        optimistic_document.rows[0].plain_text(),
        "  13.08.2026 19:32"
    );
    assert_eq!(
        optimistic_document.rows[1].plain_text(),
        "optimistic prompt"
    );

    let mut hydrated = AppState::default();
    hydrated.hydrate_visible_history(vec![VisibleHistoryRecord {
        version: 2,
        role: VisibleRole::Prompt,
        stable_id: "saved-1".into(),
        turn_id: "saved-1".into(),
        order: 1,
        text: "hydrated prompt".into(),
        attachments: Vec::new(),
        timestamp_ms: Some(1_786_638_720_000),
        text_fingerprint: fingerprint("hydrated prompt"),
        presentation: PersistedPresentation::Plain,
        rendered_text: None,
        rendered_text_fingerprint: None,
    }]);

    let hydrated_document = HistoryDocument::from_app_at_offset(&hydrated, 50, offset);

    assert_eq!(hydrated_document.rows[0].plain_text(), "  13.08.2026 19:32");
    assert_eq!(hydrated_document.rows[1].plain_text(), "hydrated prompt");
}

#[test]
fn working_prompt_is_above_composer_and_footer() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "run tests",
        Some(1),
    )));
    app.agent_status = AgentStatus::Working;
    app.working_since = Some(Instant::now() - Duration::from_secs(2));
    let editor = Editor::default();

    let rendered = render_to_string(&app, &editor, 80, 24);

    let prompt = rendered.find("run tests").unwrap();
    let working = rendered.find("Working (2s · esc to interrupt)").unwrap();
    let composer = rendered.find("Write a prompt").unwrap();
    assert!(prompt < working && working < composer);
}

#[test]
fn composer_shows_attached_images_before_submission() {
    let mut app = AppState::default();
    let mut editor = Editor::default();
    editor.insert_attachment(Attachment {
        id: "image-1".into(),
        display: "screen.png".into(),
        native_path: None,
    });
    editor.insert_paste("describe it");
    app.draft_attachments = editor.attachments();
    app.native_composer = NativeComposerState::OwnedAttachments(1);

    let rendered = render_to_string(&app, &editor, 80, 24);

    assert!(
        rendered.contains("[Image #1] describe it"),
        "the marker holds its place in the line, as the native composer shows it"
    );
}

#[test]
fn composer_marks_images_as_pending_until_native_verification() {
    let mut app = AppState::default();
    app.pending_attachments.push(Attachment {
        id: "pending-1".into(),
        display: "screen.png".into(),
        native_path: Some("/private/tmp/screen.png".into()),
    });

    let rendered = render_to_string(&app, &Editor::default(), 80, 24);

    assert!(rendered.contains("screen.png (verifying…)"));
}

#[test]
fn only_normalized_messages_reach_the_view() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "hello", Some(1))));
    app.apply(AppEvent::NativeFinal(Message::text("a1", "done", Some(2))));

    let rendered = render_to_string(&app, &Editor::default(), 80, 24);

    assert!(rendered.contains("hello"));
    assert!(rendered.contains("done"));
    assert!(!rendered.contains("tool_call"));
    assert!(!rendered.contains("reasoning"));
}

#[test]
fn native_ansi_white_brightness_matches_ratatui_cells() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "colors", Some(1))));
    app.apply(AppEvent::NativeFinal(Message {
        stable_id: "a1".into(),
        text: "ab".into(),
        presentation: MessagePresentation::NativeAnsi(StyledText {
            text: "ab".into(),
            runs: vec![
                style_run(0..1, Some(AnsiColor::White), Some(AnsiColor::BrightWhite)),
                style_run(1..2, Some(AnsiColor::BrightWhite), Some(AnsiColor::White)),
            ],
        }),
        attachments: Vec::new(),
        timestamp_ms: Some(2),
    }));

    let buffer = rendered_buffer(&app, 40, 14);
    let a = find_cell(&buffer, 40, 14, "a");
    let b = find_cell(&buffer, 40, 14, "b");
    assert_eq!(buffer[a].style().fg, Some(Color::Gray));
    assert_eq!(buffer[a].style().bg, Some(Color::White));
    assert_eq!(buffer[b].style().fg, Some(Color::White));
    assert_eq!(buffer[b].style().bg, Some(Color::Gray));
}

fn style_run(
    range: Range<usize>,
    foreground: Option<AnsiColor>,
    background: Option<AnsiColor>,
) -> StyleRun {
    StyleRun {
        start_byte: range.start,
        end_byte: range.end,
        foreground,
        background,
        modifiers: StyleModifiers::default(),
    }
}

fn find_cell(buffer: &Buffer, width: u16, height: u16, symbol: &str) -> (u16, u16) {
    for y in 0..height {
        for x in 0..width {
            if buffer[(x, y)].symbol() == symbol {
                return (x, y);
            }
        }
    }
    panic!("expected {symbol:?} in rendered buffer");
}

#[test]
fn history_starts_at_the_bottom_and_page_up_moves_toward_older_turns() {
    let mut app = AppState::default();
    for index in 0..20 {
        app.apply(AppEvent::NativeUser(Message::text(
            format!("u{index}"),
            format!("prompt {index}"),
            Some(index),
        )));
        app.apply(AppEvent::NativeFinal(Message::text(
            format!("a{index}"),
            format!("answer {index}"),
            Some(index),
        )));
    }

    let newest = render_to_string(&app, &Editor::default(), 50, 12);
    assert!(newest.contains("prompt 19"));
    assert!(!newest.contains("prompt 0"));

    app.scroll_from_bottom = usize::MAX;
    let oldest = render_to_string(&app, &Editor::default(), 50, 12);
    assert!(oldest.contains("prompt 0"));
    assert!(!oldest.contains("prompt 19"));
}

fn row_text(buffer: &Buffer, y: u16, width: u16) -> String {
    (0..width).map(|x| buffer[(x, y)].symbol()).collect()
}

fn gutter_column(buffer: &Buffer, width: u16, rows: Range<u16>) -> String {
    rows.map(|y| buffer[(width - 1, y)].symbol()).collect()
}

/// A history taller than the pane has to say so: the reader cannot be expected
/// to guess that there is more above, or how to get back to the newest answer.
#[test]
fn a_scrollable_history_shows_a_thumb_and_names_the_way_back() {
    let mut app = AppState::default();
    for index in 0..20 {
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
    let (width, height) = (50, 12);

    let following = rendered_buffer(&app, width, height);
    let bar = gutter_column(&following, width, 0..height - 4);
    assert!(
        bar.contains('▐'),
        "a scrollable history must show a thumb: {bar:?}"
    );
    assert!(
        !row_text(&following, height - 1, width).contains('↑'),
        "a view at the newest answer has no distance to report"
    );

    app.scroll_from_bottom = usize::MAX;
    let scrolled = rendered_buffer(&app, width, height);
    let footer = row_text(&scrolled, height - 1, width);
    assert!(footer.contains('↑'), "{footer:?}");
    assert!(footer.contains("Shift+End for the latest"), "{footer:?}");
    assert_ne!(
        gutter_column(&scrolled, width, 0..height - 4),
        bar,
        "the thumb has to move with the view"
    );
}

/// The bar is a statement about the history, not decoration: a conversation
/// that fits leaves the gutters exactly as they were.
#[test]
fn a_history_that_fits_leaves_the_gutters_clear() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "question", None)));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1", "answer", None,
    )));

    let buffer = rendered_buffer(&app, 40, 20);

    for y in 0..20 {
        if buffer[(0, y)].style().bg == Some(Color::Rgb(52, 53, 54)) {
            continue;
        }
        assert_clear_cell(&buffer, 39, y);
    }
}

#[test]
fn narrow_multiword_answer_scrolls_to_its_real_last_visual_row() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "question",
        Some(1),
    )));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "one two three four five six seven eight nine ten eleven twelve",
        Some(2),
    )));

    let rendered = render_to_string(&app, &Editor::default(), 18, 8);
    assert!(rendered.contains("twelve"));
}

#[test]
fn wrapped_prompt_rows_fill_the_full_terminal_width() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "a deliberately long prompt that wraps onto continuation rows",
        None,
    )));

    let width = 22;
    let height = 12;
    let buffer = rendered_buffer(&app, width, height);
    let document = HistoryDocument::from_app(&app, width - 2);
    let first = (0..height)
        .find(|&row| buffer[(1, row)].symbol() == "a")
        .expect("prompt row should be visible");
    let gray_rows = document
        .rows
        .iter()
        .take_while(|row| row.fill.is_some())
        .count() as u16;
    let block_start = first - 1;
    assert_eq!(buffer[(1, block_start)].symbol(), " ");
    assert_eq!(buffer[(1, block_start + gray_rows - 1)].symbol(), " ");
    for row in block_start..block_start + gray_rows {
        for column in 0..width {
            assert_eq!(
                buffer[(column, row)].style().bg,
                Some(Color::Rgb(52, 53, 54)),
            );
        }
    }
}

#[test]
fn sticky_prompt_background_reaches_both_terminal_edges() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "sticky question",
        None,
    )));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "one two three four five six seven eight nine ten eleven twelve thirteen fourteen",
        None,
    )));

    let buffer = rendered_buffer(&app, 20, 9);

    for y in 0..=1 {
        assert_prompt_fill_cell(&buffer, 0, y);
        assert_prompt_fill_cell(&buffer, 19, y);
    }
    assert_eq!(buffer[(1, 1)].symbol(), "s");
}

#[test]
fn wrapped_prompt_fills_the_band_less_the_scroll_column_without_a_role_indent() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "abcdefghijklmnopqrst",
        None,
    )));

    let document = HistoryDocument::from_app(&app, 14);
    assert_eq!(document.rows[0].plain_text(), "");
    assert_eq!(document.rows[1].plain_text(), "abcdefghijklm");
    assert_eq!(document.rows[2].plain_text(), "nopqrst");
    assert_eq!(document.rows[3].plain_text(), "");
}

#[test]
fn visual_rows_preserve_unicode_width_and_style_runs() {
    let bold = CellStyle {
        modifiers: herdr_simple_prompts::style::StyleModifiers {
            bold: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let rows = wrap_styled(
        &herdr_simple_prompts::style::StyledText {
            text: "界界a\nnext".into(),
            runs: vec![herdr_simple_prompts::style::StyleRun {
                start_byte: 0,
                end_byte: "界界".len(),
                foreground: None,
                background: None,
                modifiers: bold.modifiers,
            }],
        },
        4,
    );

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0].plain_text(), "界界");
    assert_eq!(rows[0].cell_width(), 4);
    assert_eq!(rows[0].spans[0].style, bold);
    assert_eq!(rows[1].plain_text(), "a");
    assert_eq!(rows[2].plain_text(), "next");
}

#[test]
fn sticky_prompt_keeps_top_padding_and_two_content_rows_when_space_allows() {
    let sections = [PromptSection {
        start_row: 0,
        content_start_row: 1,
        prompt_rows: 2,
        end_row: 9,
    }];
    assert_eq!(sticky_overlay(&sections, 1, 5), None);
    assert_eq!(
        sticky_overlay(&sections, 2, 5),
        Some(StickyRows {
            source_start: 0,
            screen_start: 0,
            count: 3,
        })
    );
}

#[test]
fn later_prompt_pushes_sticky_copy_off_one_row_at_a_time() {
    let sections = [
        PromptSection {
            start_row: 0,
            content_start_row: 1,
            prompt_rows: 4,
            end_row: 10,
        },
        PromptSection {
            start_row: 10,
            content_start_row: 11,
            prompt_rows: 1,
            end_row: 14,
        },
    ];
    assert_eq!(
        sticky_overlay(&sections, 7, 5),
        Some(StickyRows {
            source_start: 0,
            screen_start: 0,
            count: 3,
        })
    );
    assert_eq!(
        sticky_overlay(&sections, 8, 5),
        Some(StickyRows {
            source_start: 1,
            screen_start: 0,
            count: 2,
        })
    );
    assert_eq!(
        sticky_overlay(&sections, 9, 5),
        Some(StickyRows {
            source_start: 2,
            screen_start: 0,
            count: 1,
        })
    );
    assert_eq!(sticky_overlay(&sections, 10, 4), None);
}

#[test]
fn generated_document_rows_above_u16_max_keep_manual_viewport_and_sticky_rows() {
    let mut document = HistoryDocument {
        rows: (0..70_010)
            .map(|index| VisualRow::plain(format!("row {index}")))
            .collect(),
        prompts: vec![PromptSection {
            start_row: 70_000,
            content_start_row: 70_001,
            prompt_rows: 2,
            end_row: 70_006,
        }],
    };
    document.rows[70_001] = VisualRow::plain("prompt first");
    document.rows[70_002] = VisualRow::plain("prompt second");

    assert_eq!(document.viewport(3, 0)[0].plain_text(), "row 70007");
    assert_eq!(
        document
            .viewport(3, 4)
            .iter()
            .map(VisualRow::plain_text)
            .collect::<Vec<_>>(),
        ["prompt first", "prompt second", "row 70005"]
    );
    assert_eq!(
        sticky_overlay(&document.prompts, 70_003, 3),
        Some(StickyRows {
            source_start: 70_001,
            screen_start: 0,
            count: 2,
        })
    );
}

#[test]
fn wrapper_preserves_many_adjacent_and_gapped_runs_across_wrapped_unicode() {
    let rows = wrap_styled(
        &herdr_simple_prompts::style::StyledText {
            text: "界a\n b界c".into(),
            runs: vec![
                style_run(0..3, Some(AnsiColor::Red), None),
                style_run(3..4, Some(AnsiColor::Green), None),
                style_run(6..7, Some(AnsiColor::Blue), None),
                style_run(7..10, Some(AnsiColor::Yellow), None),
                style_run(10..11, Some(AnsiColor::Magenta), None),
            ],
        },
        3,
    );

    let flattened = rows
        .iter()
        .flat_map(|row| {
            row.spans.iter().flat_map(|span| {
                span.text
                    .chars()
                    .map(move |character| (character, span.style))
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        flattened,
        vec![
            (
                '界',
                CellStyle {
                    foreground: Some(AnsiColor::Red),
                    ..Default::default()
                },
            ),
            (
                'a',
                CellStyle {
                    foreground: Some(AnsiColor::Green),
                    ..Default::default()
                },
            ),
            (' ', CellStyle::default()),
            (
                'b',
                CellStyle {
                    foreground: Some(AnsiColor::Blue),
                    ..Default::default()
                },
            ),
            (
                '界',
                CellStyle {
                    foreground: Some(AnsiColor::Yellow),
                    ..Default::default()
                },
            ),
            (
                'c',
                CellStyle {
                    foreground: Some(AnsiColor::Magenta),
                    ..Default::default()
                },
            ),
        ]
    );
}

#[test]
fn sticky_short_viewports_prioritize_content_before_top_padding() {
    let sections = [PromptSection {
        start_row: 0,
        content_start_row: 1,
        prompt_rows: 2,
        end_row: 6,
    }];
    assert_eq!(sticky_overlay(&sections, 2, 1), None);
    assert_eq!(
        sticky_overlay(&sections, 2, 2),
        Some(StickyRows {
            source_start: 1,
            screen_start: 0,
            count: 1,
        })
    );
    assert_eq!(
        sticky_overlay(&sections, 2, 3),
        Some(StickyRows {
            source_start: 1,
            screen_start: 0,
            count: 2,
        })
    );
    assert_eq!(
        sticky_overlay(&sections, 2, 4),
        Some(StickyRows {
            source_start: 0,
            screen_start: 0,
            count: 3,
        })
    );

    let document = HistoryDocument {
        rows: (0..6)
            .map(|index| VisualRow::plain(format!("row {index}")))
            .collect(),
        prompts: sections.to_vec(),
    };
    for height in 1..=3 {
        let viewport = document.viewport(height, 3);
        assert_eq!(viewport.len(), height.min(document.rows.len()));
        assert!(
            viewport
                .iter()
                .any(|row| row.plain_text().starts_with("row"))
        );
    }
}

#[test]
fn wrapper_handles_cjk_and_combining_marks() {
    let rows = wrap_styled(
        &herdr_simple_prompts::style::StyledText {
            text: "界e\u{301}界".into(),
            ..Default::default()
        },
        3,
    );
    assert_eq!(
        rows.iter().map(VisualRow::plain_text).collect::<Vec<_>>(),
        ["界e\u{301}", "界"]
    );
}

#[test]
fn image_only_prompt_is_available_as_sticky_context() {
    let mut app = AppState::default();
    let mut image = Message::text("u1", "", Some(1));
    image.attachments.push(Attachment {
        id: "image-1".into(),
        display: "diagram.png".into(),
        native_path: None,
    });
    app.apply(AppEvent::NativeUser(image));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "one two three four five six seven eight nine ten eleven twelve",
        Some(2),
    )));

    let rendered = render_to_string(&app, &Editor::default(), 20, 9);
    assert!(rendered.contains("[Image #1]"));
}

#[test]
fn compact_paste_marker_is_not_reconstructed_in_history() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "[Pasted Content · 1000 chars]",
        Some(1),
    )));
    let rendered = render_to_string(&app, &Editor::default(), 50, 10);
    assert!(rendered.contains("[Pasted Content · 1000 chars]"));
}

#[test]
fn bottom_offset_uses_document_rows_without_second_wrapping() {
    let document = HistoryDocument {
        rows: (0..8)
            .map(|index| VisualRow::plain(format!("row {index}")))
            .collect(),
        prompts: Vec::new(),
    };
    assert_eq!(document.viewport(3, 0)[0].plain_text(), "row 5");
    assert_eq!(document.viewport(3, 2)[0].plain_text(), "row 3");
}

#[test]
fn disabled_composer_explains_that_the_source_must_be_reopened() {
    let app = AppState {
        input_enabled: false,
        connection_error: Some("source agent session changed".into()),
        ..AppState::default()
    };

    let rendered = render_to_string(&app, &Editor::default(), 80, 24);

    assert!(rendered.contains("Input disabled"));
    assert!(rendered.contains("source agent session changed"));
}

#[test]
fn occupied_native_composer_hides_plugin_draft_and_preserves_history() {
    let mut app = AppState {
        native_composer: NativeComposerState::Occupied,
        ..AppState::default()
    };
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "visible history",
        Some(1),
    )));
    app.draft_attachments.push(Attachment {
        id: "image-1".into(),
        display: "screen.png".into(),
        native_path: None,
    });
    let mut editor = Editor::default();
    editor.insert_paste("private plugin draft");

    let rendered = render_to_string(&app, &editor, 80, 18);

    assert!(rendered.contains("Native composer contains unsent input · prefix+m to return"));
    assert!(rendered.contains("visible history"));
    assert!(!rendered.contains("private plugin draft"));
    assert!(!rendered.contains("screen.png"));
}

#[test]
fn unknown_native_composer_shows_conservative_warning() {
    let app = AppState {
        native_composer: NativeComposerState::Unknown,
        ..AppState::default()
    };

    let rendered = render_to_string(&app, &Editor::default(), 80, 12);

    assert!(rendered.contains("Unable to verify native composer · prefix+m to return"));
    assert!(!rendered.contains("Write a prompt"));
}

#[test]
fn composer_shows_large_paste_marker_instead_of_log_body() {
    let app = AppState::default();
    let mut editor = Editor::default();
    editor.insert_char('>');
    editor.insert_paste(&"private-log-line\n".repeat(1_000));
    editor.insert_char('<');

    let rendered = render_to_string(&app, &editor, 80, 24);

    assert!(rendered.contains("Pasted Content"));
    assert!(rendered.contains("chars"));
    assert!(rendered.contains('>'));
    assert!(rendered.contains('<'));
    assert!(!rendered.contains("private-log-line"));
}

#[test]
fn markdown_fallback_body_styles_flow_into_rendered_visual_rows() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "show markdown",
        Some(1),
    )));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "# Heading [docs](https://example.test)\nplain **Ω** and `λ`",
        Some(2),
    )));

    let document = HistoryDocument::from_app(&app, 50);
    let visible = document
        .rows
        .iter()
        .map(VisualRow::plain_text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(visible.contains("Heading docs"));
    assert!(!visible.contains("https://example.test"));
    assert!(!visible.contains("**"));
    assert!(!visible.contains('`'));
    let omega = document
        .rows
        .iter()
        .flat_map(|row| &row.spans)
        .find(|span| span.text.contains('Ω'))
        .expect("strong Markdown contents should reach visual rows");
    assert!(omega.style.modifiers.bold);
    let lambda = document
        .rows
        .iter()
        .flat_map(|row| &row.spans)
        .find(|span| span.text.contains('λ'))
        .expect("inline code Markdown contents should reach visual rows");
    assert_eq!(lambda.style.foreground, Some(AnsiColor::Rgb(177, 185, 249)));
    assert_eq!(lambda.style.background, None);
    assert_eq!(
        app.turns[0].final_answer.as_ref().unwrap().presentation,
        MessagePresentation::MarkdownFallback
    );
}

#[test]
fn markdown_hyperlinks_survive_unicode_wrapping_as_ephemeral_spans() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "show links",
        None,
    )));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "до [документы](https://example.test/путь) после",
        None,
    )));

    let document = HistoryDocument::from_app(&app, 7);
    let linked = document
        .rows
        .iter()
        .flat_map(|row| &row.spans)
        .filter(|span| span.hyperlink.is_some())
        .collect::<Vec<_>>();

    assert_eq!(
        linked
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>(),
        "документы"
    );
    assert!(linked.iter().all(|span| {
        span.hyperlink.as_deref() == Some("https://example.test/путь")
            && span.style.foreground == Some(AnsiColor::Cyan)
            && span.style.modifiers.underline
    }));
}

#[test]
fn unsupported_markdown_schemes_render_as_ordinary_answer_text() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "show links",
        None,
    )));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "[mail](mailto:user@example.test)",
        None,
    )));

    let document = HistoryDocument::from_app(&app, 50);
    let mail = document
        .rows
        .iter()
        .flat_map(|row| &row.spans)
        .find(|span| span.text.contains("mail"))
        .expect("plain Markdown label should reach visual rows");

    assert_eq!(mail.text, "mail");
    assert_eq!(mail.hyperlink, None);
    assert_eq!(mail.style.foreground, Some(AnsiColor::BrightWhite));
    assert!(!mail.style.modifiers.underline);
}

#[test]
fn exact_native_presentation_adds_link_target_without_replacing_native_style() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "show native",
        None,
    )));
    app.apply(AppEvent::NativeFinal(Message {
        stable_id: "a1".into(),
        text: "[docs](https://example.test)".into(),
        presentation: MessagePresentation::NativeAnsi(StyledText {
            text: "docs".into(),
            runs: vec![StyleRun {
                start_byte: 0,
                end_byte: "docs".len(),
                foreground: Some(AnsiColor::Green),
                background: None,
                modifiers: StyleModifiers {
                    bold: true,
                    ..StyleModifiers::default()
                },
            }],
        }),
        attachments: Vec::new(),
        timestamp_ms: None,
    }));

    let document = HistoryDocument::from_app(&app, 50);
    let docs = document
        .rows
        .iter()
        .flat_map(|row| &row.spans)
        .find(|span| span.text.contains("docs"))
        .expect("native link label should reach visual rows");

    assert_eq!(docs.hyperlink.as_deref(), Some("https://example.test"));
    assert_eq!(docs.style.foreground, Some(AnsiColor::Green));
    assert!(docs.style.modifiers.bold);
}

#[test]
fn exact_native_non_http_links_lose_only_their_captured_underline() {
    for (index, destination) in [
        "mailto:user@example.test",
        "editor://open/project",
        "https://example.test/\u{1b}]8;;injected",
    ]
    .into_iter()
    .enumerate()
    {
        let mut app = AppState::default();
        app.apply(AppEvent::NativeUser(Message::text(
            format!("u{index}"),
            "show native",
            None,
        )));
        app.apply(AppEvent::NativeFinal(Message {
            stable_id: format!("a{index}"),
            text: format!("[label]({destination})"),
            presentation: MessagePresentation::NativeAnsi(StyledText {
                text: "label".into(),
                runs: vec![StyleRun {
                    start_byte: 0,
                    end_byte: "label".len(),
                    foreground: Some(AnsiColor::Blue),
                    background: None,
                    modifiers: StyleModifiers {
                        bold: true,
                        underline: true,
                        ..StyleModifiers::default()
                    },
                }],
            }),
            attachments: Vec::new(),
            timestamp_ms: None,
        }));

        let document = HistoryDocument::from_app(&app, 50);
        let label = document
            .rows
            .iter()
            .flat_map(|row| &row.spans)
            .find(|span| span.text.contains("label"))
            .expect("native non-clickable label should reach visual rows");

        assert_eq!(label.hyperlink, None, "destination: {destination:?}");
        assert!(label.style.modifiers.bold, "destination: {destination:?}");
        assert!(
            !label.style.modifiers.underline,
            "destination: {destination:?}"
        );
        assert_eq!(
            label.style.foreground,
            Some(AnsiColor::Blue),
            "destination: {destination:?}"
        );
    }
}

#[test]
fn terminal_draw_emits_balanced_osc_8_and_restores_the_composer_cursor() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "show link", None)));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "[OpenAI](https://openai.com)",
        None,
    )));

    let (buffer, cursor) = render_terminal_to_buffer(&app, &Editor::default(), 50, 14);
    let expected = "\u{1b}]8;;https://openai.com\u{7}OpenAI\u{1b}]8;;\u{7}";
    let symbols = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>();

    assert!(symbols.contains(&expected));
    assert_eq!(
        symbols
            .iter()
            .filter(|symbol| symbol.contains("https://openai.com"))
            .count(),
        1
    );
    assert_eq!(cursor, (1, 11));
}

#[test]
fn terminal_draw_emits_balanced_local_file_osc_8() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "show link", None)));
    app.apply(AppEvent::NativeFinal(Message::final_text(
        "a1",
        "[TDD](/Users/example/skills/тест/SKILL.md)",
        None,
    )));

    let (buffer, _) = render_terminal_to_buffer(&app, &Editor::default(), 90, 14);
    let expected = concat!(
        "\u{1b}]8;;file:///Users/example/skills/тест/SKILL.md\u{7}",
        "/Users/example/skills/тест/SKILL.md",
        "\u{1b}]8;;\u{7}",
    );
    let symbols = buffer
        .content
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>();

    assert!(symbols.contains(&expected));
    assert_eq!(
        symbols
            .iter()
            .filter(|symbol| symbol.contains("file:///Users/example/skills/тест/SKILL.md"))
            .count(),
        1
    );
}

#[test]
fn native_presentation_renders_owned_visible_text_not_canonical_markdown() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "show native",
        Some(1),
    )));
    app.apply(AppEvent::NativeFinal(Message {
        stable_id: "a1".into(),
        text: "# [canonical](https://example.test)".into(),
        presentation: MessagePresentation::NativeAnsi(StyledText {
            text: "Native label".into(),
            runs: vec![StyleRun {
                start_byte: 0,
                end_byte: "Native label".len(),
                foreground: Some(AnsiColor::Green),
                background: None,
                modifiers: StyleModifiers {
                    bold: true,
                    ..StyleModifiers::default()
                },
            }],
        }),
        attachments: Vec::new(),
        timestamp_ms: Some(2),
    }));

    let document = HistoryDocument::from_app(&app, 50);
    let rendered = render_to_string(&app, &Editor::default(), 50, 14);
    let native = document
        .rows
        .iter()
        .flat_map(|row| &row.spans)
        .find(|span| span.text.contains("Native label"))
        .expect("native rendered text should reach visual rows");
    assert_eq!(native.style.foreground, Some(AnsiColor::Green));
    assert!(native.style.modifiers.bold);
    assert!(rendered.contains("Native label"));
    assert!(!rendered.contains("canonical"));
    assert!(!rendered.contains("https://example.test"));

    let buffer = rendered_buffer(&app, 50, 14);
    let cell = find_cell(&buffer, 50, 14, "N");
    assert_eq!(buffer[cell].style().fg, Some(Color::Green));
    assert!(buffer[cell].style().add_modifier.contains(Modifier::BOLD));
}

#[test]
fn blocked_view_replaces_history_working_row_and_composer_with_native_surface() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "history must be hidden",
        Some(1),
    )));
    app.agent_status = AgentStatus::Blocked;
    app.working_since = Some(Instant::now() - Duration::from_secs(2));
    app.blocked_surface = Some(Ok(StyledText {
        text: "Allow command?\n  Yes\n  No".into(),
        runs: Vec::new(),
    }));
    let mut editor = Editor::default();
    editor.insert_char('d');
    editor.insert_char('r');
    editor.insert_char('a');
    editor.insert_char('f');
    editor.insert_char('t');

    let rendered = render_to_string(&app, &editor, 80, 16);

    assert!(rendered.contains("INTERACTION REQUIRED"));
    assert!(rendered.contains("Allow command?"));
    assert!(rendered.contains("Native Codex/Claude interaction · prefix+m to return"));
    assert!(!rendered.contains("history must be hidden"));
    assert!(!rendered.contains("Working ("));
    assert!(!rendered.contains("draft"));
}

#[test]
fn blocked_view_uses_the_same_clear_horizontal_gutters() {
    let mut app = AppState {
        agent_status: AgentStatus::Blocked,
        ..AppState::default()
    };
    app.blocked_surface = Some(Ok(StyledText {
        text: "Allow command?\n  Yes\n  No".into(),
        runs: Vec::new(),
    }));

    let buffer = rendered_buffer(&app, 32, 8);

    assert_clear_horizontal_gutters(&buffer, 32, 8);
    assert_eq!(buffer[(1, 0)].symbol(), "I");
    assert_eq!(buffer[(1, 1)].symbol(), "A");
}

#[test]
fn sub_three_cell_widths_render_without_painting_or_panicking() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text("u1", "prompt", None)));

    for width in 1..3 {
        let buffer = rendered_buffer(&app, width, 8);
        for y in 0..8 {
            for x in 0..width {
                assert_clear_cell(&buffer, x, y);
            }
        }
    }
}

#[test]
fn blocked_snapshot_styles_are_sanitized_and_confined_to_body() {
    let mut app = AppState {
        agent_status: AgentStatus::Blocked,
        ..AppState::default()
    };
    app.blocked_surface = Some(Ok(herdr_simple_prompts::ansi::sanitize_ansi(
        "\u{1b}]0;rewrite-title\u{7}\u{1b}[31;44mDANGER\u{1b}[0m",
    )));

    let buffer = render_to_buffer(&app, &Editor::default(), 72, 8);
    let rendered = render_to_string(&app, &Editor::default(), 72, 8);
    let header = find_cell(&buffer, 72, 8, "I");
    let danger = (1, 1);
    let footer = (1, 7);

    assert!(!rendered.contains("rewrite-title"));
    assert_eq!(buffer[header].style().fg, Some(Color::Yellow));
    assert!(buffer[header].style().add_modifier.contains(Modifier::BOLD));
    assert_eq!(buffer[danger].style().fg, Some(Color::Red));
    assert_eq!(buffer[danger].style().bg, Some(Color::Blue));
    assert_ne!(buffer[footer].style().bg, Some(Color::Blue));
}

#[test]
fn blocked_snapshot_failure_shows_owned_fallback_and_return_hint() {
    let app = AppState {
        agent_status: AgentStatus::Blocked,
        blocked_surface: Some(Err("socket unavailable".into())),
        ..AppState::default()
    };

    let rendered = render_to_string(&app, &Editor::default(), 80, 8);

    assert!(rendered.contains("Unable to read native interaction"));
    assert!(rendered.contains("prefix+m"));
    assert!(!rendered.contains("socket unavailable"));
}

#[test]
fn leaving_blocked_view_restores_the_exact_ordinary_content() {
    let mut app = AppState::default();
    app.apply(AppEvent::NativeUser(Message::text(
        "u1",
        "ordinary history",
        Some(1),
    )));
    let mut editor = Editor::default();
    editor.insert_paste("unchanged draft");
    let ordinary = render_to_string(&app, &editor, 80, 14);

    app.agent_status = AgentStatus::Blocked;
    app.blocked_surface = Some(Ok(StyledText {
        text: "Choose one".into(),
        runs: Vec::new(),
    }));
    let blocked = render_to_string(&app, &editor, 80, 14);
    app.update_blocked_surface(AgentStatus::Done, None);
    let restored = render_to_string(&app, &editor, 80, 14);

    assert!(blocked.contains("Choose one"));
    assert_eq!(restored, ordinary);
}

/// The marker under the cursor is drawn as one piece, the way the native
/// composer draws it.
#[test]
fn the_image_under_the_cursor_is_drawn_highlighted() {
    let mut app = AppState::default();
    let mut editor = Editor::default();
    editor.insert_attachment(Attachment {
        id: "image-1".into(),
        display: "Image #12".into(),
        native_path: None,
    });
    editor.insert_paste("describe it");
    app.draft_attachments = editor.attachments();
    app.native_composer = NativeComposerState::OwnedAttachments(1);
    editor.move_document_start();

    let buffer = render_to_buffer(&app, &editor, 60, 20);
    let highlighted = buffer
        .content
        .iter()
        .filter(|cell| cell.modifier.contains(ratatui::style::Modifier::REVERSED))
        .map(|cell| cell.symbol().to_owned())
        .collect::<String>();

    assert_eq!(highlighted, "[Image #12]");
}

/// A pane asked to attach or drop an image answers in its own time. The wait is
/// named and counted, so a composer that sits still for a few seconds reads as
/// waiting rather than as broken.
#[test]
fn a_wait_on_the_pane_is_named_and_counted() {
    let mut app = AppState::default();
    app.pending_action = Some(herdr_simple_prompts::app::PendingAction::new(
        "Attaching image",
    ));

    let rendered = render_to_string(&app, &Editor::default(), 60, 20);

    assert!(
        rendered.contains("Attaching image (0s)…"),
        "the overlay says what it is waiting for: {rendered}"
    );
}
