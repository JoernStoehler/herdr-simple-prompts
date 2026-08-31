use herdr_simple_prompts::agent::AgentKind;
use herdr_simple_prompts::composer::{
    ComposerAccess, NativeComposerState, classify_native_composer, native_composer_parts,
    native_composer_text,
};
use herdr_simple_prompts::style::{AnsiColor, StyleModifiers, StyleRun, StyledText};

fn styled_range(text: &str, needle: &str, foreground: AnsiColor, dim: bool) -> StyledText {
    let start_byte = text.find(needle).expect("fixture contains styled text");
    StyledText {
        text: text.to_owned(),
        runs: vec![StyleRun {
            start_byte,
            end_byte: start_byte + needle.len(),
            foreground: Some(foreground),
            background: None,
            modifiers: StyleModifiers {
                dim,
                ..StyleModifiers::default()
            },
        }],
    }
}

fn plain(text: &str) -> StyledText {
    StyledText {
        text: text.to_owned(),
        runs: Vec::new(),
    }
}

fn codex_surface(prompt: &str) -> String {
    format!("────────\n• answer\n────────\n› {prompt}\ngpt-5.6-sol xhigh · /repo · weekly 75% left")
}

fn codex_working_surface(prompt: &str, elapsed: &str, separator: char, suffix: &str) -> String {
    format!(
        "• Working ({elapsed} {separator} {suffix})\n› {prompt}\ngpt-5.6-sol xhigh · /repo · weekly 47% left"
    )
}

fn claude_surface(prompt: &str) -> String {
    format!("⏺ answer\n────────────────\n❯ {prompt}\n────────────────\nClaude Opus · /repo")
}

#[test]
fn codex_dim_only_placeholder_is_clear() {
    let text = codex_surface("Write a prompt");
    let start_byte = text.find("Write a prompt").unwrap();
    let surface = StyledText {
        text,
        runs: vec![StyleRun {
            start_byte,
            end_byte: start_byte + "Write a prompt".len(),
            foreground: None,
            background: None,
            modifiers: StyleModifiers {
                dim: true,
                ..StyleModifiers::default()
            },
        }],
    };

    assert_eq!(
        classify_native_composer(AgentKind::Codex, &surface),
        NativeComposerState::Clear
    );
}

#[test]
fn codex_dim_suggestion_is_clear_without_matching_literal_copy() {
    let text = codex_surface("Summarize recent commits");
    let surface = styled_range(
        &text,
        "Summarize recent commits",
        AnsiColor::Indexed(8),
        false,
    );

    assert_eq!(
        classify_native_composer(AgentKind::Codex, &surface),
        NativeComposerState::Clear
    );
}

#[test]
fn codex_plain_text_is_occupied() {
    assert_eq!(
        classify_native_composer(AgentKind::Codex, &plain(&codex_surface("unsent text"))),
        NativeComposerState::Occupied
    );
}

#[test]
fn codex_exact_image_tokens_are_counted() {
    assert_eq!(
        classify_native_composer(
            AgentKind::Codex,
            &plain(&codex_surface("[Image #1]  [Image #2]")),
        ),
        NativeComposerState::OwnedAttachments(2)
    );
}

#[test]
fn codex_image_token_mixed_with_text_is_occupied() {
    assert_eq!(
        classify_native_composer(
            AgentKind::Codex,
            &plain(&codex_surface("[Image #1] explain this")),
        ),
        NativeComposerState::Occupied
    );
}

#[test]
fn codex_missing_footer_and_truncated_surface_are_unknown() {
    assert_eq!(
        classify_native_composer(AgentKind::Codex, &plain("────────\n• answer\n────────\n› "),),
        NativeComposerState::Unknown
    );
    assert_eq!(
        classify_native_composer(AgentKind::Codex, &plain("• answer\n────────")),
        NativeComposerState::Unknown
    );
}

#[test]
fn historical_codex_prompt_followed_by_a_new_block_is_not_current() {
    let surface = plain(concat!(
        "────────\n",
        "› old text\n",
        "• Ran command\n",
        "────────\n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    ));

    assert_eq!(
        classify_native_composer(AgentKind::Codex, &surface),
        NativeComposerState::Unknown
    );
}

#[test]
fn claude_prompt_box_classifies_clear_text_and_images() {
    let clear_text = claude_surface("Write a prompt");
    let clear = styled_range(&clear_text, "Write a prompt", AnsiColor::BrightBlack, false);
    assert_eq!(
        classify_native_composer(AgentKind::Claude, &clear),
        NativeComposerState::Clear
    );
    assert_eq!(
        classify_native_composer(AgentKind::Claude, &plain(&claude_surface("unsent text")),),
        NativeComposerState::Occupied
    );
    assert_eq!(
        classify_native_composer(
            AgentKind::Claude,
            &plain(&claude_surface("[Image #7]\n  [Image #9]")),
        ),
        NativeComposerState::OwnedAttachments(2)
    );
}

#[test]
fn claude_historical_prompt_box_followed_by_output_is_unknown() {
    let surface = plain(concat!(
        "────────────────\n",
        "❯ [Image #1]\n",
        "────────────────\n",
        "⏺ later answer\n",
        "Claude Opus · /repo",
    ));

    assert_eq!(
        classify_native_composer(AgentKind::Claude, &surface),
        NativeComposerState::Unknown
    );
}

#[test]
fn claude_requires_both_prompt_box_rules() {
    assert_eq!(
        classify_native_composer(
            AgentKind::Claude,
            &plain("⏺ answer\n────────────────\n❯ \nClaude Opus · /repo"),
        ),
        NativeComposerState::Unknown
    );
}

#[test]
fn arbitrary_rgb_text_is_not_treated_as_a_placeholder() {
    let text = codex_surface("Summarize recent commits");
    let surface = styled_range(
        &text,
        "Summarize recent commits",
        AnsiColor::Rgb(65, 66, 67),
        true,
    );

    assert_eq!(
        classify_native_composer(AgentKind::Codex, &surface),
        NativeComposerState::Occupied
    );
}

/// Nothing above the composer gates classification.
///
/// Codex prints notices there — a weekly-limit warning, taken here from a live
/// pane — and demanding a rule or an elapsed label above the composer made
/// every pane carrying one unverifiable, so the overlay refused all input.
#[test]
fn codex_ignores_whatever_sits_above_the_composer() {
    for above in [
        "─ Worked for 2m 3s ────────",
        "─ Worked for eventually ────────",
        "⚠ Heads up, you have less than 10% of your weekly limit left. Run /status for a breakdown.",
        "╰──────────────────────────────",
        "• Working (2s • esc to interrupt)",
    ] {
        let surface = plain(&format!(
            "• answer\n{above}\n› \ngpt-5.6-sol xhigh · /repo · weekly 75% left"
        ));
        assert_eq!(
            classify_native_composer(AgentKind::Codex, &surface),
            NativeComposerState::Clear,
            "line above the composer must not gate: {above:?}"
        );
    }
}

#[test]
fn codex_working_boundary_accepts_a_dim_placeholder() {
    let text = codex_working_surface("Write a prompt", "10m 20s", '•', "esc to interrupt");
    let start_byte = text.find("Write a prompt").unwrap();
    let surface = StyledText {
        text,
        runs: vec![StyleRun {
            start_byte,
            end_byte: start_byte + "Write a prompt".len(),
            foreground: None,
            background: None,
            modifiers: StyleModifiers {
                dim: true,
                ..StyleModifiers::default()
            },
        }],
    };

    assert_eq!(
        classify_native_composer(AgentKind::Codex, &surface),
        NativeComposerState::Clear
    );
}

#[test]
fn codex_working_boundary_still_detects_unsent_text() {
    let surface = plain(&codex_working_surface(
        "unsent native text",
        "2s",
        '•',
        "esc to interrupt",
    ));

    assert_eq!(
        classify_native_composer(AgentKind::Codex, &surface),
        NativeComposerState::Occupied
    );
}

/// Without a footer there is nothing to pin the composer to, so classification
/// still fails closed rather than guessing.
#[test]
fn codex_without_a_footer_fails_closed() {
    for surface in [
        "• Working (2s • esc to interrupt)\n› Write a prompt".to_owned(),
        "• answer\n────────\n› Write a prompt".to_owned(),
        "• answer\n────────\n› Write a prompt\nnot a footer at all".to_owned(),
        codex_working_surface("Write a prompt", "2m 3s", '•', "esc to interrupt").replace(
            "gpt-5.6-sol xhigh · /repo · weekly 47% left",
            "trailing prose",
        ),
    ] {
        assert_eq!(
            classify_native_composer(AgentKind::Codex, &plain(&surface)),
            NativeComposerState::Unknown,
            "surface must fail closed: {surface:?}"
        );
    }
}

#[test]
fn access_policy_requires_an_exact_attachment_count() {
    assert_eq!(NativeComposerState::Clear.access(0), ComposerAccess::Ready);
    assert_eq!(
        NativeComposerState::Clear.access(1),
        ComposerAccess::Occupied
    );
    assert_eq!(
        NativeComposerState::OwnedAttachments(2).access(2),
        ComposerAccess::Ready
    );
    assert_eq!(
        NativeComposerState::OwnedAttachments(2).access(1),
        ComposerAccess::Occupied
    );
    assert_eq!(
        NativeComposerState::Occupied.access(0),
        ComposerAccess::Occupied
    );
    assert_eq!(
        NativeComposerState::Unknown.access(0),
        ComposerAccess::Unknown
    );
}

/// A live Claude pane: an empty composer separated by U+00A0, a closing rule
/// and a mode hint instead of a footer.
///
/// Requiring `"❯ "` and a `model · cwd` footer left the overlay unable to
/// verify the composer, so it refused every keystroke.
#[test]
fn shipping_claude_chrome_classifies_the_composer() {
    let surface = |composer: &str| {
        plain(&format!(
            "⏺ answer\n\n✳ Topsy-turvying… (35s · ↓ 1.9k tokens)\n────────────────────────────────\n{composer}\n────────────────────────────────\n  ⏵⏵ accept edits on (shift+tab to cycle) · esc to interrupt · ← for agents"
        ))
    };

    assert_eq!(
        classify_native_composer(AgentKind::Claude, &surface("❯\u{a0}")),
        NativeComposerState::Clear,
    );
    assert_eq!(
        classify_native_composer(AgentKind::Claude, &surface("❯")),
        NativeComposerState::Clear,
    );
    assert_eq!(
        classify_native_composer(AgentKind::Claude, &surface("❯ unsent text")),
        NativeComposerState::Occupied,
    );
    assert_eq!(
        classify_native_composer(AgentKind::Claude, &surface("❯ [Image #1]")),
        NativeComposerState::OwnedAttachments(1),
    );
}

/// Text sitting beside an image cannot be taken over: the overlay cannot carry
/// the image, and clearing the composer to take the text would destroy it.
#[test]
fn a_composer_mixing_text_and_images_is_not_offered_for_adoption() {
    let mixed = plain(concat!(
        "• answer\n",
        "────────\n",
        "› [Image #1] describe it\n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    ));
    assert_eq!(native_composer_text(AgentKind::Codex, &mixed), None);

    let text_only = plain(concat!(
        "• answer\n",
        "────────\n",
        "› describe it\n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    ));
    assert_eq!(
        native_composer_text(AgentKind::Codex, &text_only).as_deref(),
        Some("describe it"),
    );
}

/// An image pasted with no prompt beside it still has to reach the overlay, or
/// the overlay sees a pane holding something it does not know about and refuses
/// every keystroke — which is exactly what a freshly pasted image looked like.
#[test]
fn a_composer_holding_only_images_is_offered_for_adoption() {
    let surface = |content: &str| {
        plain(&format!(
            "• answer\n────────\n› {content}\ngpt-5.6-sol xhigh · /repo · weekly 75% left"
        ))
    };

    let parts = native_composer_parts(AgentKind::Codex, &surface("[Image #1]"))
        .expect("a bare image must be adopted");
    assert_eq!(parts.markers, [1]);
    assert_eq!(parts.text, "");

    let two = native_composer_parts(AgentKind::Codex, &surface("[Image #1] [Image #2]"))
        .expect("several bare images must be adopted");
    assert_eq!(two.markers, [1, 2]);
    assert_eq!(two.text, "");

    let mixed = native_composer_parts(AgentKind::Codex, &surface("[Image #1] describe it"))
        .expect("an image beside text must be adopted");
    assert_eq!(mixed.markers, [1]);
    assert_eq!(mixed.text, "describe it");

    assert_eq!(native_composer_parts(AgentKind::Codex, &surface(" ")), None);
}

/// A custom `statusLine` adds a chrome line below the composer's closing rule.
///
/// Requiring a single trailing line left every pane configured with one
/// classified `Unknown`, and the overlay refused all typing with
/// `Unable to verify native composer`. Shape taken from a live pane.
#[test]
fn claude_composer_is_verifiable_under_a_custom_status_line() {
    let rule = "─".repeat(32);
    let surface = format!(
        "● answer\n{rule}\n❯\u{a0}\n{rule}\n  Opus 5 (1M context)  ~/…/staff-roster-row-edit-v4  fix/staff-roster-row-edit-v4  ctx 49%\n  ⏵⏵ auto mode on (shift+tab to cycle) · ← for agents"
    );

    assert_eq!(
        classify_native_composer(AgentKind::Claude, &plain(&surface)),
        NativeComposerState::Clear
    );
}

/// The shipping build opens an authored line with `●`; older ones used `⏺`.
#[test]
fn claude_answers_below_the_composer_stay_unverifiable_under_either_bullet() {
    let rule = "─".repeat(32);
    for bullet in ['\u{23fa}', '\u{25cf}'] {
        let surface = format!("{rule}\n❯\u{a0}\n{rule}\n{bullet} an older answer");
        assert_eq!(
            classify_native_composer(AgentKind::Claude, &plain(&surface)),
            NativeComposerState::Unknown,
            "bullet {bullet:?} must still mark the line as the agent's"
        );
    }
}
