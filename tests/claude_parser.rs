use herdr_simple_prompts::agent::claude::ClaudeAdapter;
use herdr_simple_prompts::model::ConversationEvent;

fn ingest_fixture(adapter: &mut ClaudeAdapter, path: &str) -> Vec<ConversationEvent> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .enumerate()
        .flat_map(|(index, line)| adapter.ingest_line(index as u64 + 1, line).unwrap())
        .collect()
}

#[test]
fn simple_answer_is_committed_only_when_the_turn_finishes() {
    let mut adapter = ClaudeAdapter::default();
    let events = ingest_fixture(&mut adapter, "tests/fixtures/claude/simple.jsonl");

    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], ConversationEvent::User(message)
            if message.text == "hello claude"
                && message.timestamp_ms == Some(1_786_528_800_000)));
    assert!(matches!(
        adapter.finalize_pending(),
        Some(ConversationEvent::Final(message)) if message.text == "Hello back."
    ));
}

#[test]
fn tool_cycle_commits_only_terminal_visible_text() {
    let mut adapter = ClaudeAdapter::default();
    let events = ingest_fixture(&mut adapter, "tests/fixtures/claude/tool_cycle.jsonl");

    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], ConversationEvent::User(message) if message.text == "fix it"));
    assert!(matches!(
        adapter.finalize_pending(),
        Some(ConversationEvent::Final(message)) if message.text == "Fixed and tested."
    ));
}

#[test]
fn excludes_meta_thinking_progress_and_sidechains_but_keeps_images() {
    let mut adapter = ClaudeAdapter::default();
    let events = ingest_fixture(&mut adapter, "tests/fixtures/claude/filtered.jsonl");

    assert_eq!(events.len(), 1);
    let ConversationEvent::User(message) = &events[0] else {
        panic!("expected a user event");
    };
    assert_eq!(message.text, "look at this");
    assert_eq!(message.attachments.len(), 1);
    assert!(adapter.finalize_pending().is_none());
}

#[test]
fn keeps_visible_text_from_a_mixed_thinking_response() {
    let mut adapter = ClaudeAdapter::default();
    adapter
        .ingest_line(
            1,
            r#"{"type":"user","uuid":"u1","message":{"content":"question"}}"#,
        )
        .unwrap();
    adapter
        .ingest_line(
            2,
            r#"{"type":"assistant","uuid":"a1","message":{"content":[{"type":"thinking","thinking":"private"},{"type":"text","text":"Visible answer."}]}}"#,
        )
        .unwrap();

    assert!(matches!(
        adapter.finalize_pending(),
        Some(ConversationEvent::Final(message)) if message.text == "Visible answer."
    ));
}

#[test]
fn preserves_native_compact_paste_marker_exactly() {
    let mut adapter = ClaudeAdapter::default();
    let text = "inspect\n[Pasted Content 1000 chars]";
    let line = serde_json::json!({
        "type": "user",
        "uuid": "u1",
        "message": {"content": text},
    })
    .to_string();

    let events = adapter.ingest_line(1, &line).unwrap();

    assert!(matches!(
        events.as_slice(),
        [ConversationEvent::User(message)] if message.text == text
    ));
}

#[test]
fn keeps_prompts_queued_while_the_agent_works() {
    let mut adapter = ClaudeAdapter::default();
    let events = ingest_fixture(&mut adapter, "tests/fixtures/claude/queued.jsonl");

    let ConversationEvent::User(first) = &events[0] else {
        panic!("expected the native prompt first");
    };
    assert_eq!(first.text, "start the audit");
    assert!(matches!(&events[1], ConversationEvent::Final(message)
            if message.text == "Reading the files now."));
    let ConversationEvent::User(queued) = &events[2] else {
        panic!("expected the queued prompt");
    };
    assert_eq!(queued.text, "[Image #3] look at this too");
    assert_eq!(queued.attachments.len(), 1);
    assert_eq!(queued.timestamp_ms, Some(1_786_528_860_000));
    assert!(matches!(&events[3], ConversationEvent::User(message)
            if message.text == "and the queued one landed"));
    assert!(matches!(&events[4], ConversationEvent::User(message)
            if message.text == "and stop after that" && message.attachments.is_empty()));
    assert_eq!(events.len(), 5, "task notifications must stay hidden");
    assert!(matches!(
        adapter.finalize_pending(),
        Some(ConversationEvent::Final(message)) if message.text == "Both handled."
    ));
}

#[test]
fn hides_a_task_notification_dequeued_as_a_user_record() {
    let mut adapter = ClaudeAdapter::default();
    let line = serde_json::json!({
        "type": "user",
        "uuid": "n1",
        "origin": {"kind": "task-notification"},
        "promptSource": "system",
        "message": {"role": "user", "content": "<task-notification>done</task-notification>"},
    })
    .to_string();

    assert!(adapter.ingest_line(1, &line).unwrap().is_empty());
}

#[test]
fn keeps_a_message_relayed_from_a_coordinator_session() {
    let mut adapter = ClaudeAdapter::default();
    let line = serde_json::json!({
        "type": "user",
        "uuid": "c1",
        "origin": {"kind": "coordinator"},
        "message": {"role": "user", "content": "The coordinator sent a message: fix the P1."},
    })
    .to_string();

    assert!(matches!(
        adapter.ingest_line(1, &line).unwrap().as_slice(),
        [ConversationEvent::User(message)]
            if message.text == "The coordinator sent a message: fix the P1."
    ));
}
