use super::time::record_timestamp_ms;
use crate::model::{Attachment, ConversationEvent, Message};
use crate::{AppError, AppResult};
use serde_json::Value;

#[derive(Default)]
pub struct ClaudeAdapter {
    active_user: bool,
    pending_final: Option<Message>,
}

impl ClaudeAdapter {
    pub fn ingest_line(
        &mut self,
        line_number: u64,
        line: &str,
    ) -> AppResult<Vec<ConversationEvent>> {
        let record: Value = serde_json::from_str(line).map_err(|error| {
            AppError::new(
                "Claude transcript",
                format!("invalid JSON on line {line_number}: {error}"),
            )
        })?;
        Ok(self.ingest_value(line_number, &record))
    }

    pub fn ingest_value(&mut self, line_number: u64, record: &Value) -> Vec<ConversationEvent> {
        if truthy(record, "isSidechain") || truthy(record, "isMeta") {
            return Vec::new();
        }
        match record.get("type").and_then(Value::as_str) {
            Some("user") => self.parse_user(line_number, record),
            Some("assistant") => {
                self.parse_assistant(line_number, record);
                Vec::new()
            }
            Some("attachment") => self.parse_queued_prompt(line_number, record),
            _ => Vec::new(),
        }
    }

    pub fn finalize_pending(&mut self) -> Option<ConversationEvent> {
        self.pending_final.take().map(ConversationEvent::Final)
    }

    fn parse_user(&mut self, line_number: u64, record: &Value) -> Vec<ConversationEvent> {
        let Some(content) = record.pointer("/message/content") else {
            return Vec::new();
        };
        if contains_block(content, "tool_result") || is_system_prompt(record) {
            return Vec::new();
        }
        self.emit_user(line_number, record, content)
    }

    // A prompt typed while the agent is working is usually stored once, as a
    // `queued_command` attachment holding the same blocks the model was given,
    // rather than as a `user` record, so this is the only place those prompts
    // can be read from. Queued commands the person did not type - a background
    // task notification, for one - stay hidden.
    fn parse_queued_prompt(&mut self, line_number: u64, record: &Value) -> Vec<ConversationEvent> {
        let Some(attachment) = record.get("attachment") else {
            return Vec::new();
        };
        if attachment.get("type").and_then(Value::as_str) != Some("queued_command")
            || attachment.get("commandMode").and_then(Value::as_str) != Some("prompt")
            || !matches!(
                attachment.pointer("/origin/kind").and_then(Value::as_str),
                None | Some("human")
            )
        {
            return Vec::new();
        }
        let Some(prompt) = attachment.get("prompt") else {
            return Vec::new();
        };
        self.emit_user(line_number, record, prompt)
    }

    fn emit_user(
        &mut self,
        line_number: u64,
        record: &Value,
        content: &Value,
    ) -> Vec<ConversationEvent> {
        let text = visible_text(content);
        let attachments = image_attachments(content, line_number);
        if text.trim().is_empty() && attachments.is_empty() {
            return Vec::new();
        }
        let mut events = Vec::new();
        if let Some(final_answer) = self.finalize_pending() {
            events.push(final_answer);
        }
        self.active_user = true;
        events.push(ConversationEvent::User(Message {
            stable_id: stable_id(line_number, record),
            text,
            presentation: crate::style::MessagePresentation::Plain,
            attachments,
            timestamp_ms: record_timestamp_ms(record),
        }));
        events
    }

    fn parse_assistant(&mut self, line_number: u64, record: &Value) {
        if !self.active_user {
            return;
        }
        let Some(content) = record.pointer("/message/content") else {
            return;
        };
        if contains_block(content, "tool_use") {
            return;
        }
        let text = visible_text(content);
        if text.trim().is_empty() {
            return;
        }
        self.pending_final = Some(Message::text(
            stable_id(line_number, record),
            text,
            record_timestamp_ms(record),
        ));
    }
}

// Claude Code writes prompts of its own into the transcript. A finished
// background command is queued as a `queued_command` attachment and then
// dequeued as a `user` record whose body is indistinguishable from a typed
// prompt; only the envelope tells them apart - `promptSource: "system"`, with
// `origin.kind` naming the injector. A message relayed from a coordinator
// session is still someone addressing this session, so it is not a system
// prompt and stays visible.
fn is_system_prompt(record: &Value) -> bool {
    record.get("promptSource").and_then(Value::as_str) == Some("system")
        || record.pointer("/origin/kind").and_then(Value::as_str) == Some("task-notification")
}

fn visible_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.to_owned(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn contains_block(content: &Value, block_type: &str) -> bool {
    content
        .as_array()
        .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == block_type))
}

fn image_attachments(content: &Value, line_number: u64) -> Vec<Attachment> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("image"))
        .enumerate()
        .map(|(index, _)| Attachment {
            id: format!("claude:{line_number}:image:{}", index + 1),
            display: format!("Image #{}", index + 1),
            native_path: None,
        })
        .collect()
}

fn stable_id(line_number: u64, record: &Value) -> String {
    record
        .get("uuid")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("claude:{line_number}"))
}

fn truthy(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool) == Some(true)
}
