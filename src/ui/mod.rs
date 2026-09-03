pub mod interaction;
pub mod render;
mod runtime;
mod terminal;
pub mod visual_rows;

use crate::agent::follower::{FollowerEvent, TranscriptFollower};
use crate::agent::{
    AgentKind, AgentPaths, AgentStatus, TranscriptAdapter, agent_identity, resolve_transcript,
};
use crate::ansi::sanitize_ansi;
use crate::app::{AppEvent, AppState, PendingAction};
use crate::composer::{
    ComposerAccess, NativeComposerState, classify_native_composer, native_attachment_markers,
    native_composer_parts,
};
use crate::editor::{Editor, staged_image_path};
use crate::herdr::HerdrClient;
use crate::history::HistoryWriter;
use crate::model::{Attachment, ConversationEvent};
use crate::state::{DraftWriter, StateStore};
use crate::status::extract_status;
use crate::{AppError, AppResult};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use interaction::{map_interaction_key, map_interaction_paste};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use runtime::{RuntimeEvent, UiRuntime};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use terminal::TerminalGuard;

const DRAFT_DEBOUNCE: Duration = Duration::from_millis(250);
const ADOPT_ATTEMPTS: usize = 8;
const ADOPT_RETRY_DELAY: Duration = Duration::from_millis(60);
const ADOPT_KEY_BATCH: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapturePolicy {
    NewestFinalOnly,
    AllFinals,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DraftChange {
    None,
    Debounced,
    Immediate,
}

pub fn run_from_env() -> AppResult<()> {
    let source_pane = required_env("HERDR_SIMPLE_PROMPTS_SOURCE_PANE")?;
    let socket = required_env("HERDR_SOCKET_PATH")?;
    let state_root = required_env("HERDR_PLUGIN_STATE_DIR")?;
    let client = HerdrClient::connect(Path::new(&socket))
        .map_err(|error| AppError::new("ui", error.to_string()))?;
    let state_store = StateStore::at(state_root);
    let identity = state_store.with_lifecycle_lock(|| {
        state_store.validate_saved_namespaces(&client, now_ms())?;
        let identity = agent_identity(&client, &source_pane)?;
        state_store.bind_verified_namespace(&source_pane, &identity.session_id, now_ms())?;
        Ok(identity)
    })?;
    // An agent that has not been prompted yet has no transcript on disk. That
    // is a "not yet", not a failure: the overlay opens on an empty history and
    // the follower picks the file up when the first prompt creates it. Refusing
    // to start meant a freshly opened agent could not be viewed at all.
    let paths = AgentPaths::from_env()?;
    let kind = identity.kind;
    let session_id = identity.session_id.clone();
    let open_transcript = move || {
        let path = resolve_transcript(kind, &session_id, &paths)?;
        let adapter: Box<dyn TranscriptAdapter> = match kind {
            AgentKind::Codex => Box::new(crate::agent::codex::CodexAdapter),
            AgentKind::Claude => Box::new(crate::agent::claude::ClaudeAdapter::default()),
        };
        TranscriptFollower::new(path, adapter)
    };
    let mut follower = open_transcript().ok();
    let history_journal = state_store.history_journal(&source_pane, &identity.session_id)?;
    let saved_history = history_journal.load()?;
    let mut history_writer = Some(HistoryWriter::spawn(history_journal));
    let mut editor = Editor::default();
    let mut draft = state_store.load_draft(&source_pane)?;
    draft
        .prompt_displays
        .retain(|summary| summary.session_id == identity.session_id);
    let mut draft_writer = Some(DraftWriter::spawn(
        state_store.clone(),
        source_pane.clone(),
        Some(identity.session_id.clone()),
    ));
    editor.replace_snapshot(draft.editor);
    let native = inspect_native_composer(
        identity.kind,
        editor.is_blank(),
        || {
            client
                .pane_read_visible_ansi(&source_pane, 200)
                .map_err(|error| AppError::new("native draft", error.to_string()))
        },
        |keys| {
            client
                .pane_send_input(&source_pane, None, keys)
                .map(|_| ())
                .map_err(|error| AppError::new("native draft", error.to_string()))
        },
        ADOPT_ATTEMPTS,
        ADOPT_RETRY_DELAY,
    );
    let adopt_notice = match native {
        Ok(view) => {
            // A saved draft can outlive the images its markers point at, so the
            // markers are measured against the pane before anything is drawn.
            if let Some(held) = view.attachments.as_deref() {
                editor.sync_attachments(held);
            }
            view.adopted.and_then(|adopted| {
                editor.replace(adopted.text);
                // The markers precede the text in the pane, so they take the
                // same place here.
                editor.move_document_start();
                for marker in &adopted.markers {
                    editor.insert_attachment(Attachment {
                        id: format!("native-image-{marker}"),
                        display: format!("Image #{marker}"),
                        native_path: None,
                    });
                }
                editor.move_document_end();
                (!adopted.cleared)
                    .then(|| "native composer still holds a copy of the adopted draft".to_owned())
            })
        }
        Err(error) => Some(error.to_string()),
    };

    let mut app = AppState {
        session_id: identity.session_id.clone(),
        agent_status: identity.status,
        native_composer: NativeComposerState::Unknown,
        working_since: identity.status.is_working().then(Instant::now),
        draft_attachments: editor.attachments(),
        prompt_displays: draft.prompt_displays,
        ..AppState::default()
    };
    app.hydrate_visible_history(saved_history);
    app.background_error = adopt_notice;
    let mut history_cache = render::HistoryRenderCache::default();
    let initial_events = match follower.as_mut() {
        Some(follower) => follower.poll_initial(identity.status)?,
        None => Vec::new(),
    };
    let runtime = UiRuntime::spawn(
        Path::new(&socket),
        identity.clone(),
        follower,
        Box::new(open_transcript),
    )?;
    apply_follower_events_with_policy(
        &mut app,
        initial_events,
        &mut history_cache,
        &runtime,
        CapturePolicy::NewestFinalOnly,
    );
    queue_history_upserts(&mut app, history_writer.as_ref());
    draft_writer
        .as_ref()
        .expect("draft writer exists")
        .queue_editor(editor.snapshot(), app.prompt_displays.clone());
    let mut stdout = io::stdout();
    let _guard = TerminalGuard::enter(&mut stdout)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let mut local_sequence = 1_u64;
    let mut draft_dirty = false;
    let mut draft_save_at = Instant::now();

    loop {
        while let Some(event) = runtime.try_recv() {
            if matches!(&event, RuntimeEvent::SourcePaneClosed) {
                if let Some(writer) = history_writer.take() {
                    writer.cancel();
                }
                drop(draft_writer.take());
                if let Err(error) =
                    state_store.with_lifecycle_lock(|| state_store.remove_pane_state(&source_pane))
                {
                    app.transcript_error = Some(format!("source cleanup: {error}"));
                }
                app.source_pane_closed();
                draft_dirty = false;
                continue;
            }
            let change = apply_runtime_event(
                event,
                &identity,
                &mut app,
                &mut editor,
                &mut history_cache,
                &runtime,
            );
            queue_history_upserts(&mut app, history_writer.as_ref());
            if let Some(writer) = draft_writer.as_ref() {
                apply_draft_change(
                    change,
                    writer,
                    &app,
                    &editor,
                    &mut draft_dirty,
                    &mut draft_save_at,
                );
            }
        }
        if draft_dirty && Instant::now() >= draft_save_at {
            if let Some(writer) = draft_writer.as_ref() {
                writer.queue_editor(editor.snapshot(), app.prompt_displays.clone());
            }
            draft_dirty = false;
        }
        if let Some(error) = draft_writer.as_ref().and_then(DraftWriter::take_error) {
            app.background_error = Some(format!("draft: {error}"));
        }
        if let Some(error) = history_writer.as_ref().and_then(HistoryWriter::take_error) {
            app.background_error = Some(format!("history: {error}"));
        }

        app.expire_notice();
        render::draw_terminal(&mut terminal, &app, &editor, &mut history_cache)?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        match event::read()? {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                let change = handle_key(
                    key,
                    &mut app,
                    &mut editor,
                    &runtime,
                    &mut local_sequence,
                    &mut history_cache,
                )?;
                if let Some(writer) = draft_writer.as_ref() {
                    apply_draft_change(
                        change,
                        writer,
                        &app,
                        &editor,
                        &mut draft_dirty,
                        &mut draft_save_at,
                    );
                }
            }
            Event::Paste(content) => {
                if handle_blocked_paste(&content, &mut app, &runtime) {
                    continue;
                }
                let change = handle_ordinary_paste(
                    &content,
                    &mut app,
                    &mut editor,
                    &runtime,
                    &mut local_sequence,
                );
                if let Some(writer) = draft_writer.as_ref() {
                    apply_draft_change(
                        change,
                        writer,
                        &app,
                        &editor,
                        &mut draft_dirty,
                        &mut draft_save_at,
                    );
                }
            }
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn handle_blocked_paste(content: &str, app: &mut AppState, runtime: &UiRuntime) -> bool {
    if app.agent_status != AgentStatus::Blocked {
        return false;
    }
    if let Err(error) = runtime.forward_interaction(map_interaction_paste(content)) {
        app.interaction_error = Some(error.to_string());
    }
    true
}

fn handle_ordinary_paste(
    content: &str,
    app: &mut AppState,
    editor: &mut Editor,
    runtime: &UiRuntime,
    local_sequence: &mut u64,
) -> DraftChange {
    if !ordinary_input_allowed(app) {
        return DraftChange::None;
    }
    if let Some(path) = staged_image_path(content) {
        let attachment = Attachment {
            id: next_image_id(local_sequence),
            display: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            native_path: Some(path.clone()),
        };
        match runtime.forward_staged_image(attachment.clone(), path) {
            Ok(()) => {
                app.pending_action = Some(PendingAction::new("Attaching image"));
                app.pending_attachments.push(attachment);
            }
            Err(error) => app.send_error = Some(error.to_string()),
        }
        DraftChange::None
    } else {
        editor.insert_paste(content);
        DraftChange::Debounced
    }
}

fn handle_key(
    key: KeyEvent,
    app: &mut AppState,
    editor: &mut Editor,
    runtime: &UiRuntime,
    local_sequence: &mut u64,
    history_cache: &mut render::HistoryRenderCache,
) -> AppResult<DraftChange> {
    // Reading comes before everything else. These chords mean nothing to a
    // composer or to a native question, so they are answered here and never
    // forwarded: whatever the agent is doing, the conversation stays readable.
    if let Some(scroll) = history_scroll_key(key) {
        apply_history_scroll(scroll, app, history_cache);
        return Ok(DraftChange::None);
    }
    if app.agent_status == AgentStatus::Blocked {
        if let Some(input) = map_interaction_key(key)
            && let Err(error) = runtime.forward_interaction(input)
        {
            app.interaction_error = Some(error.to_string());
        }
        return Ok(DraftChange::None);
    }
    match key.code {
        KeyCode::Up if scrolls_history(app, editor) => {
            history_cache.scroll_up(1);
            app.scroll_from_bottom = history_cache.scroll_from_bottom();
            return Ok(DraftChange::None);
        }
        KeyCode::Down if scrolls_history(app, editor) => {
            history_cache.scroll_down(1);
            app.scroll_from_bottom = history_cache.scroll_from_bottom();
            return Ok(DraftChange::None);
        }
        KeyCode::Esc if app.agent_status == AgentStatus::Working => {
            if let Err(error) = runtime.interrupt() {
                app.send_error = Some(error.to_string());
            }
            return Ok(DraftChange::None);
        }
        _ => {}
    }
    if !ordinary_input_allowed(app) {
        return Ok(DraftChange::None);
    }
    let change = match (key.code, key.modifiers) {
        (KeyCode::Enter, modifiers)
            if modifiers.contains(KeyModifiers::SHIFT)
                || modifiers.contains(KeyModifiers::CONTROL) =>
        {
            editor.newline();
            DraftChange::Debounced
        }
        (KeyCode::Char('j'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            editor.newline();
            DraftChange::Debounced
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if !app.pending_attachments.is_empty() {
                app.send_error = Some("wait for image attachment verification".to_owned());
                return Ok(DraftChange::None);
            }
            if editor.submission_text().trim().is_empty() && app.draft_attachments.is_empty() {
                return Ok(DraftChange::None);
            }
            let submission = editor.take_editor_submission();
            let complete_text = submission.complete_text.clone();
            app.send_error = None;
            let attachments = app.draft_attachments.clone();
            let expected_attachments = attachments.len();
            let local_id = format!("local-{}", *local_sequence);
            *local_sequence += 1;
            app.apply(AppEvent::PromptSubmitted {
                local_id: local_id.clone(),
                submission,
                attachments,
                at_ms: now_ms(),
            });
            history_cache.invalidate();
            if let Err(error) =
                runtime.submit(local_id.clone(), complete_text, expected_attachments)
            {
                app.apply(AppEvent::SendFailed {
                    local_id,
                    reason: error.to_string(),
                });
                history_cache.invalidate();
                editor.replace_snapshot(app.draft.clone());
                app.send_error = Some(error.to_string());
            }
            DraftChange::Immediate
        }
        (KeyCode::Char('v'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            let attachment = Attachment {
                id: next_image_id(local_sequence),
                display: format!("Image #{}", app.draft_attachments.len() + 1),
                native_path: None,
            };
            match runtime.forward_local_image(attachment.clone()) {
                Ok(()) => app.pending_attachments.push(attachment),
                Err(error) => app.send_error = Some(error.to_string()),
            }
            DraftChange::None
        }
        (KeyCode::Left, modifiers) if modifiers.contains(KeyModifiers::SUPER) => {
            editor.move_home();
            DraftChange::None
        }
        (KeyCode::Right, modifiers) if modifiers.contains(KeyModifiers::SUPER) => {
            editor.move_end();
            DraftChange::None
        }
        (KeyCode::Up, modifiers) if modifiers.contains(KeyModifiers::SUPER) => {
            editor.move_document_start();
            DraftChange::None
        }
        (KeyCode::Down, modifiers) if modifiers.contains(KeyModifiers::SUPER) => {
            editor.move_document_end();
            DraftChange::None
        }
        // Killing to a line end. macOS sends `^U` for command+backspace, which
        // is what actually arrives; the super arms cover terminals configured
        // to forward the modifier itself.
        (KeyCode::Backspace, modifiers) if modifiers.contains(KeyModifiers::SUPER) => {
            clear_to_line_start(app, editor, runtime)
        }
        (KeyCode::Char('u'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            clear_to_line_start(app, editor, runtime)
        }
        (KeyCode::Delete, modifiers) if modifiers.contains(KeyModifiers::SUPER) => {
            editor.delete_to_line_end();
            DraftChange::Debounced
        }
        (KeyCode::Char('k'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            editor.delete_to_line_end();
            DraftChange::Debounced
        }
        // Word editing. Terminals disagree about what the option key sends:
        // some emit a modified arrow, others the readline escapes (`alt+b`,
        // `alt+f`, `alt+d`), so both spellings are accepted.
        (KeyCode::Backspace, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.delete_word_left();
            DraftChange::Debounced
        }
        (KeyCode::Char('w'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            editor.delete_word_left();
            DraftChange::Debounced
        }
        (KeyCode::Delete, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.delete_word_right();
            DraftChange::Debounced
        }
        (KeyCode::Char('d'), modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.delete_word_right();
            DraftChange::Debounced
        }
        (KeyCode::Left, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.move_word_left();
            DraftChange::None
        }
        (KeyCode::Char('b'), modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.move_word_left();
            DraftChange::None
        }
        (KeyCode::Right, modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.move_word_right();
            DraftChange::None
        }
        (KeyCode::Char('f'), modifiers) if modifiers.contains(KeyModifiers::ALT) => {
            editor.move_word_right();
            DraftChange::None
        }
        (KeyCode::Backspace, _) => {
            // An image is not ours to drop: the picture lives in the native
            // composer, so the pane has to lose it first and say so.
            if let Some(marker) = editor
                .attachment_at_cursor()
                .or_else(|| editor.attachment_behind_cursor())
                .and_then(|attachment| {
                    marker_number(attachment).map(|number| (attachment.id.clone(), number))
                })
            {
                // One at a time. The pane works through these in order, so a
                // second request made while the first is still running names a
                // picture that is already on its way out — and comes back as a
                // failure over a removal that in fact worked. The wait is named
                // on screen, so the presses that land in it are let go of.
                if app.pending_action.is_some() {
                    return Ok(DraftChange::None);
                }
                match runtime.remove_attachment(marker.0, marker.1) {
                    Ok(()) => app.pending_action = Some(PendingAction::new("Removing image")),
                    Err(error) => app.background_error = Some(error.to_string()),
                }
                return Ok(DraftChange::None);
            }
            editor.backspace();
            DraftChange::Debounced
        }
        (KeyCode::Delete, _) => {
            editor.delete();
            DraftChange::Debounced
        }
        (KeyCode::Left, _) => {
            editor.move_left();
            DraftChange::None
        }
        (KeyCode::Right, _) => {
            editor.move_right();
            DraftChange::None
        }
        (KeyCode::Up, _) => {
            editor.move_up();
            DraftChange::None
        }
        (KeyCode::Down, _) => {
            editor.move_down();
            DraftChange::None
        }
        // Line and document navigation. macOS never delivers the command key
        // to a terminal application — the multiplexer has no name for that
        // modifier at all — so the readline bindings are the ones that reach
        // us, and the super arms fire only where a terminal is configured to
        // send the modifier through.
        (KeyCode::Char('a'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            editor.move_home();
            DraftChange::None
        }
        (KeyCode::Char('e'), modifiers) if modifiers.contains(KeyModifiers::CONTROL) => {
            editor.move_end();
            DraftChange::None
        }
        (KeyCode::Home, _) => {
            editor.move_home();
            DraftChange::None
        }
        (KeyCode::End, _) => {
            editor.move_end();
            DraftChange::None
        }
        (KeyCode::Char(character), modifiers)
            if modifiers.is_empty() || modifiers == KeyModifiers::SHIFT =>
        {
            app.send_error = None;
            editor.insert_char(character);
            DraftChange::Debounced
        }
        _ => return Ok(DraftChange::None),
    };
    Ok(change)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AdoptedDraft {
    text: String,
    markers: Vec<usize>,
    cleared: bool,
}

/// Takes over whatever the native composer is holding.
///
/// Opening the overlay over a half-written prompt used to mean switching back
/// to finish it, because the overlay refuses to send while the native composer
/// is occupied. Move the text here instead — and clear it there, since leaving
/// a copy behind would splice it onto the next prompt.
///
/// The text is returned even when clearing fails, so a draft is never lost to a
/// half-finished takeover; the caller says so and the guard stays on.
/// What the pane is holding, read once.
///
/// The count and the adoption have to come from the same reading. Taken from
/// two, a redraw between them can retire the very marker just adopted, leaving
/// the overlay disagreeing with the pane about how many images exist — which it
/// answers by guarding its own input.
#[derive(Debug)]
struct NativeComposerView {
    attachments: Option<Vec<usize>>,
    adopted: Option<AdoptedDraft>,
}

fn inspect_native_composer(
    kind: crate::agent::AgentKind,
    may_adopt: bool,
    mut read: impl FnMut() -> AppResult<String>,
    press: impl FnMut(&[&str]) -> AppResult<()>,
    attempts: usize,
    retry_delay: Duration,
) -> AppResult<NativeComposerView> {
    let surface = sanitize_ansi(&read()?);
    let attachments = native_attachment_markers(kind, &surface);
    let parts = may_adopt
        .then(|| native_composer_parts(kind, &surface))
        .flatten();
    let adopted = match parts {
        Some(parts) => lift_native_draft(kind, parts, read, press, attempts, retry_delay)?,
        None => None,
    };
    Ok(NativeComposerView {
        attachments,
        adopted,
    })
}

/// The adoption half of [`inspect_native_composer`], for tests that care only
/// about what is taken from the pane.
#[cfg(test)]
fn adopt_native_draft(
    kind: crate::agent::AgentKind,
    read: impl FnMut() -> AppResult<String>,
    press: impl FnMut(&[&str]) -> AppResult<()>,
    attempts: usize,
    retry_delay: Duration,
) -> AppResult<Option<AdoptedDraft>> {
    inspect_native_composer(kind, true, read, press, attempts, retry_delay).map(|view| view.adopted)
}

fn lift_native_draft(
    kind: crate::agent::AgentKind,
    parts: crate::composer::ComposerParts,
    mut read: impl FnMut() -> AppResult<String>,
    mut press: impl FnMut(&[&str]) -> AppResult<()>,
    attempts: usize,
    retry_delay: Duration,
) -> AppResult<Option<AdoptedDraft>> {
    // Beside an image, only a single rendered line is taken. A wrapped or
    // multi-line draft carries newlines the buffer does not have, so counting
    // deletions from it would overshoot — and an overshoot eats the marker,
    // which is the one thing here that cannot be undone.
    if !parts.markers.is_empty() && parts.text.contains('\n') {
        return Ok(None);
    }
    if parts.text.is_empty() {
        // Nothing to lift: the images stay where they are and the overlay only
        // has to know they are there.
        return Ok(Some(AdoptedDraft {
            text: String::new(),
            markers: parts.markers,
            cleared: true,
        }));
    }
    let removal = removal_keys(&parts.text);
    for attempt in 0..attempts {
        press(&removal_prefix(parts.markers.len()))?;
        for batch in removal.chunks(ADOPT_KEY_BATCH) {
            press(batch)?;
        }
        if attempt + 1 < attempts {
            std::thread::sleep(retry_delay);
        }
        if composer_holds_only(kind, &read()?, parts.markers.len()) {
            return Ok(Some(AdoptedDraft {
                text: parts.text,
                markers: parts.markers,
                cleared: true,
            }));
        }
    }
    Ok(Some(AdoptedDraft {
        text: parts.text,
        markers: parts.markers,
        cleared: false,
    }))
}

/// Beside an image the text is removed character by character, so the markers
/// that stay are never touched. With nothing to preserve, one kill is enough.
fn removal_prefix(attachments: usize) -> Vec<&'static str> {
    if attachments == 0 {
        vec!["ctrl+e", "ctrl+u"]
    } else {
        vec!["ctrl+e"]
    }
}

fn removal_keys(text: &str) -> Vec<&'static str> {
    if text.is_empty() {
        Vec::new()
    } else {
        vec!["backspace"; text.chars().count()]
    }
}

fn composer_holds_only(kind: crate::agent::AgentKind, ansi: &str, attachments: usize) -> bool {
    classify_native_composer(kind, &sanitize_ansi(ansi)).access(attachments)
        == ComposerAccess::Ready
}

/// The number the pane printed for an image, taken back out of its label.
fn marker_number(attachment: &Attachment) -> Option<usize> {
    attachment.display.strip_prefix("Image #")?.parse().ok()
}

fn ordinary_input_allowed(app: &AppState) -> bool {
    app.input_enabled && app.composer_access() == ComposerAccess::Ready
}

/// The wheel arrives as arrow keys, so the arrows drive the history whenever
/// the composer has no text for a cursor to move through — and always while the
/// composer is guarded, so navigation never depends on the native pane.
fn scrolls_history(app: &AppState, editor: &Editor) -> bool {
    !ordinary_input_allowed(app) || editor.display_text().is_empty()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HistoryScroll {
    RowUp,
    RowDown,
    PageUp,
    PageDown,
    Oldest,
    Latest,
}

/// The chords that scroll whatever else is on screen.
///
/// The bare arrows belong to the wheel, and a composer with text keeps them for
/// its cursor — a draft must never take reading away, so the same movements
/// answer to shift as well, and `PageUp`/`PageDown` move by the view rather
/// than by a fixed count.
///
/// The ends of the history are spelled twice. `Shift+Home` and `Shift+End` are
/// the names a full keyboard has; a laptop without those keys reaches the same
/// two places with shift and alt on the arrows, which no board has to press a
/// function key for.
fn history_scroll_key(key: KeyEvent) -> Option<HistoryScroll> {
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    match key.code {
        KeyCode::PageUp => Some(HistoryScroll::PageUp),
        KeyCode::PageDown => Some(HistoryScroll::PageDown),
        KeyCode::Up if shift && alt => Some(HistoryScroll::Oldest),
        KeyCode::Down if shift && alt => Some(HistoryScroll::Latest),
        KeyCode::Up if shift => Some(HistoryScroll::RowUp),
        KeyCode::Down if shift => Some(HistoryScroll::RowDown),
        KeyCode::Home if shift => Some(HistoryScroll::Oldest),
        KeyCode::End if shift => Some(HistoryScroll::Latest),
        _ => None,
    }
}

fn apply_history_scroll(
    scroll: HistoryScroll,
    app: &mut AppState,
    history_cache: &mut render::HistoryRenderCache,
) {
    let page = history_cache.page_rows();
    match scroll {
        HistoryScroll::RowUp => history_cache.scroll_up(1),
        HistoryScroll::RowDown => history_cache.scroll_down(1),
        HistoryScroll::PageUp => history_cache.scroll_up(page),
        HistoryScroll::PageDown => history_cache.scroll_down(page),
        HistoryScroll::Oldest => history_cache.scroll_to_oldest(),
        HistoryScroll::Latest => history_cache.scroll_to_latest(),
    }
    app.scroll_from_bottom = history_cache.scroll_from_bottom();
}

/// Clears back to the start of the line, pictures and all.
///
/// A picture is not ours to drop: it lives in the native composer, so it has to
/// be asked for and waited on. The clear therefore goes in steps — the text
/// back to the nearest picture, then that picture, then whatever is behind it —
/// and carries on as each answer comes back.
fn clear_to_line_start(
    app: &mut AppState,
    editor: &mut Editor,
    runtime: &UiRuntime,
) -> DraftChange {
    let target = clear_target(app, editor);
    editor.delete_back_to(target);
    app.clearing_line_to =
        ask_for_the_next_image_in_the_way(app, editor, runtime, target).then_some(target);
    DraftChange::Debounced
}

/// Where a clear should stop: the start of the line as it is drawn.
///
/// A paragraph that has wrapped is several lines on the screen, and the one
/// being cleared is the one the cursor is on rather than the whole paragraph
/// above it. Standing at the start of a line already, the press means the break
/// itself — which closes the line up and leaves the cursor at the end of the
/// one before; where the break is only a wrap, it means the row above.
fn clear_target(app: &AppState, editor: &Editor) -> usize {
    let cursor = editor.cursor_atom();
    // Before the first draw there is no width to speak of, and a line that has
    // not been wrapped anywhere is the whole paragraph.
    let width = match app.composer_width {
        0 => usize::MAX,
        width => width,
    };
    let rows = visual_rows::wrap_plain(editor.display_text(), width);
    let (row, _) = rows.cell_of(editor.display_cursor_byte());
    let start = editor.atom_at_display(rows.row_start(row));
    if start < cursor {
        return start;
    }
    if editor.display_text()[..editor.display_cursor_byte()].ends_with('\n') {
        return cursor.saturating_sub(1);
    }
    match row.checked_sub(1) {
        Some(above) => editor.atom_at_display(rows.row_start(above)),
        None => 0,
    }
}

/// Asks the pane for the picture the clear has run into, if it has run into
/// one. Returns whether the clear is still going.
fn ask_for_the_next_image_in_the_way(
    app: &mut AppState,
    editor: &Editor,
    runtime: &UiRuntime,
    target: usize,
) -> bool {
    if editor.cursor_atom() <= target || app.pending_action.is_some() {
        return false;
    }
    let Some((id, marker)) = editor.attachment_behind_cursor().and_then(|attachment| {
        marker_number(attachment).map(|number| (attachment.id.clone(), number))
    }) else {
        return false;
    };
    match runtime.remove_attachment(id, marker) {
        Ok(()) => {
            app.pending_action = Some(PendingAction::new("Removing image"));
            true
        }
        Err(error) => {
            app.background_error = Some(error.to_string());
            false
        }
    }
}

fn apply_runtime_event(
    event: RuntimeEvent,
    original: &crate::agent::AgentIdentity,
    app: &mut AppState,
    editor: &mut Editor,
    history_cache: &mut render::HistoryRenderCache,
    runtime: &UiRuntime,
) -> DraftChange {
    match event {
        RuntimeEvent::Transcript(events) => {
            app.transcript_error = None;
            apply_follower_events_with_policy(
                app,
                events,
                history_cache,
                runtime,
                CapturePolicy::AllFinals,
            )
        }
        RuntimeEvent::TranscriptError(error) => {
            app.transcript_error = Some(error);
            DraftChange::None
        }
        RuntimeEvent::Observation(Ok(observation)) => {
            if app.source_pane_closed {
                return DraftChange::None;
            }
            app.connection_error = None;
            app.input_enabled = true;
            app.native_composer = observation.native_composer;
            let current = observation.identity;
            let was_working = app.agent_status.is_working();
            if !was_working && current.status.is_working() {
                app.working_since = Some(Instant::now());
            } else if was_working && !current.status.is_working() {
                app.working_since = None;
            }
            app.update_blocked_surface(current.status, observation.blocked_surface);
            app.status_line = Some(extract_status(
                original.kind,
                &observation.status_text,
                original.cwd.clone(),
            ));
            DraftChange::None
        }
        RuntimeEvent::Observation(Err(error)) => {
            if app.source_pane_closed {
                return DraftChange::None;
            }
            app.connection_error = Some(error);
            app.input_enabled = false;
            app.native_composer = NativeComposerState::Unknown;
            DraftChange::None
        }
        RuntimeEvent::Submitted { local_id, result } => {
            if let Err(reason) = result {
                app.apply(AppEvent::SendFailed {
                    local_id,
                    reason: reason.clone(),
                });
                history_cache.invalidate();
                editor.replace_snapshot(app.draft.clone());
                app.send_error = Some(reason);
                DraftChange::Immediate
            } else {
                DraftChange::None
            }
        }
        RuntimeEvent::Interrupted(Err(error)) => {
            app.send_error = Some(error);
            DraftChange::None
        }
        RuntimeEvent::Interrupted(Ok(())) => DraftChange::None,
        RuntimeEvent::InteractionForwarded(result) => {
            app.apply_interaction_result(result);
            DraftChange::None
        }
        RuntimeEvent::ImageForwarded { attachment, result } => {
            app.pending_action = None;
            app.pending_attachments
                .retain(|candidate| candidate.id != attachment.id);
            match result {
                Ok(marker) => {
                    // The pane names the picture; the overlay repeats that name
                    // rather than inventing one, or it would later ask for the
                    // wrong image to be removed.
                    let attachment = Attachment {
                        display: format!("Image #{marker}"),
                        ..attachment
                    };
                    editor.insert_attachment(attachment);
                    app.draft_attachments = editor.attachments();
                    DraftChange::Immediate
                }
                Err(error) => {
                    app.send_error = Some(error);
                    DraftChange::None
                }
            }
        }
        RuntimeEvent::AttachmentRemoved { id, result } => {
            app.pending_action = None;
            match result {
                Ok(()) => {
                    editor.remove_attachment(&id);
                    app.draft_attachments = editor.attachments();
                    if let Some(target) = app.clearing_line_to {
                        // The picture is gone; the clear carries on from where
                        // it stopped for it.
                        editor.delete_back_to(target);
                        app.clearing_line_to =
                            ask_for_the_next_image_in_the_way(app, editor, runtime, target)
                                .then_some(target);
                    }
                    DraftChange::Immediate
                }
                Err(error) => {
                    // A picture that would not go stops the clear: what is left
                    // on the line is what the pane still holds.
                    app.clearing_line_to = None;
                    app.background_error = Some(error);
                    DraftChange::None
                }
            }
        }
        RuntimeEvent::FinalPresentation {
            stable_id,
            text_fingerprint,
            presentation,
        } => {
            app.apply(AppEvent::FinalPresentation {
                stable_id,
                text_fingerprint,
                presentation,
            });
            history_cache.invalidate();
            DraftChange::None
        }
        RuntimeEvent::CaptureDiagnostic(error) => {
            app.transcript_error = Some(format!("final style capture: {error}"));
            DraftChange::None
        }
        RuntimeEvent::SourcePaneClosed => {
            app.source_pane_closed();
            DraftChange::None
        }
    }
}

fn apply_draft_change(
    change: DraftChange,
    writer: &DraftWriter,
    app: &AppState,
    editor: &Editor,
    dirty: &mut bool,
    save_at: &mut Instant,
) {
    match change {
        DraftChange::None => {}
        DraftChange::Debounced => {
            *dirty = true;
            *save_at = Instant::now() + DRAFT_DEBOUNCE;
        }
        DraftChange::Immediate => {
            writer.queue_editor(editor.snapshot(), app.prompt_displays.clone());
            *dirty = false;
        }
    }
}

fn queue_history_upserts(app: &mut AppState, writer: Option<&HistoryWriter>) {
    for record in app.drain_history_upserts() {
        if let Some(writer) = writer {
            writer.queue(record);
        }
    }
}

fn apply_follower_events_with_policy(
    app: &mut AppState,
    events: Vec<FollowerEvent>,
    history_cache: &mut render::HistoryRenderCache,
    runtime: &UiRuntime,
    capture_policy: CapturePolicy,
) -> DraftChange {
    let prompt_displays_before = app.prompt_displays.clone();
    let replayed = events
        .iter()
        .any(|event| matches!(event, FollowerEvent::Reloaded))
        || capture_policy == CapturePolicy::NewestFinalOnly;
    if replayed {
        app.apply(AppEvent::TranscriptReloaded);
        history_cache.invalidate();
    }
    let newest_final = (capture_policy == CapturePolicy::NewestFinalOnly)
        .then(|| {
            events.iter().rposition(|event| {
                matches!(
                    event,
                    FollowerEvent::Conversation(ConversationEvent::Final(_))
                )
            })
        })
        .flatten();
    for (event_index, event) in events.into_iter().enumerate() {
        match event {
            FollowerEvent::Conversation(ConversationEvent::User(message)) => {
                app.apply(AppEvent::NativeUser(message));
                history_cache.invalidate();
            }
            FollowerEvent::Conversation(ConversationEvent::Final(message)) => {
                let stable_id = message.stable_id.clone();
                let canonical_text = message.text.clone();
                app.apply(AppEvent::NativeFinal(message));
                history_cache.invalidate();
                let should_capture =
                    capture_policy == CapturePolicy::AllFinals || newest_final == Some(event_index);
                if should_capture
                    && let Err(error) = runtime.capture_final(stable_id, canonical_text)
                {
                    app.transcript_error = Some(error.to_string());
                }
            }
            FollowerEvent::Reloaded => {}
            FollowerEvent::ParseError { line, message } => {
                app.transcript_error = Some(format!("transcript line {line}: {message}"))
            }
        }
    }
    if replayed {
        app.apply(AppEvent::TranscriptReplayComplete);
    }
    if app.prompt_displays == prompt_displays_before {
        DraftChange::None
    } else {
        DraftChange::Immediate
    }
}

fn required_env(name: &'static str) -> AppResult<String> {
    std::env::var(name).map_err(|_| AppError::new("ui", format!("{name} is not set")))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn next_image_id(sequence: &mut u64) -> String {
    let id = format!("local-image-{}", *sequence);
    *sequence += 1;
    id
}

#[cfg(test)]
mod tests {
    use super::{
        CapturePolicy, apply_follower_events_with_policy, apply_runtime_event,
        handle_blocked_paste, handle_key, handle_ordinary_paste, render, runtime,
    };
    use crate::agent::follower::FollowerEvent;
    use crate::agent::{AgentIdentity, AgentKind, AgentStatus};
    use crate::app::AppState;
    use crate::composer::NativeComposerState;
    use crate::editor::Editor;
    use crate::history::{PersistedPresentation, VisibleHistoryRecord, VisibleRole};
    use crate::model::{Attachment, ConversationEvent, Message};
    use crate::paste::fingerprint;
    use crate::ui::interaction::InteractionInput;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::path::PathBuf;

    fn final_event(index: usize) -> FollowerEvent {
        FollowerEvent::Conversation(ConversationEvent::Final(Message::final_text(
            format!("final-{index}"),
            format!("answer-{index}"),
            Some(index as u64),
        )))
    }

    #[test]
    fn initial_replay_with_more_than_capture_capacity_enqueues_only_newest_final() {
        let (runtime, captures) = runtime::capture_test_runtime(8);
        let mut app = AppState::default();
        let mut cache = render::HistoryRenderCache::default();
        let mut events = Vec::new();
        for index in 0..10 {
            events.push(FollowerEvent::Conversation(ConversationEvent::User(
                Message::text(format!("prompt-{index}"), format!("prompt {index}"), None),
            )));
            events.push(final_event(index));
        }

        apply_follower_events_with_policy(
            &mut app,
            events,
            &mut cache,
            &runtime,
            CapturePolicy::NewestFinalOnly,
        );

        let captured: Vec<_> = captures.try_iter().collect();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].stable_id, "final-9");
        assert_eq!(app.turns.len(), 10);
    }

    /// Style capture is an enhancement layered over an answer that already
    /// renders, so a saturated queue must not surface as an error line.
    #[test]
    fn saturated_capture_queue_is_not_reported_as_an_error() {
        let (runtime, captures) = runtime::capture_test_runtime(2);
        let mut app = AppState::default();
        let mut cache = render::HistoryRenderCache::default();
        let mut events = Vec::new();
        for index in 0..5 {
            events.push(FollowerEvent::Conversation(ConversationEvent::User(
                Message::text(format!("prompt-{index}"), format!("prompt {index}"), None),
            )));
            events.push(final_event(index));
        }

        apply_follower_events_with_policy(
            &mut app,
            events,
            &mut cache,
            &runtime,
            CapturePolicy::AllFinals,
        );

        assert_eq!(captures.try_iter().count(), 2);
        assert_eq!(app.transcript_error, None);
        assert_eq!(app.turns.len(), 5);
    }

    #[test]
    fn live_batch_enqueues_each_new_final() {
        let (runtime, captures) = runtime::capture_test_runtime(8);
        let mut app = AppState::default();
        let mut cache = render::HistoryRenderCache::default();
        let events = vec![
            FollowerEvent::Conversation(ConversationEvent::User(Message::text(
                "prompt-1", "prompt 1", None,
            ))),
            final_event(1),
            FollowerEvent::Conversation(ConversationEvent::User(Message::text(
                "prompt-2", "prompt 2", None,
            ))),
            final_event(2),
        ];

        apply_follower_events_with_policy(
            &mut app,
            events,
            &mut cache,
            &runtime,
            CapturePolicy::AllFinals,
        );

        let captured: Vec<_> = captures.try_iter().collect();
        assert_eq!(captured.len(), 2);
        assert_eq!(captured[0].stable_id, "final-1");
        assert_eq!(captured[1].stable_id, "final-2");
    }

    #[test]
    fn initial_batch_reconciles_hydrated_history_in_native_replay_order() {
        let (runtime, _captures) = runtime::capture_test_runtime(8);
        let mut app = AppState::default();
        app.hydrate_visible_history(vec![
            VisibleHistoryRecord {
                version: 2,
                role: VisibleRole::Prompt,
                stable_id: "prompt-2".into(),
                turn_id: "prompt-2".into(),
                order: 1,
                text: "saved prompt 2".into(),
                attachments: Vec::new(),
                timestamp_ms: Some(1),
                text_fingerprint: fingerprint("saved prompt 2"),
                presentation: PersistedPresentation::Plain,
                rendered_text: None,
                rendered_text_fingerprint: None,
            },
            VisibleHistoryRecord {
                version: 2,
                role: VisibleRole::Prompt,
                stable_id: "saved-missing".into(),
                turn_id: "saved-missing".into(),
                order: 2,
                text: "retain me".into(),
                attachments: Vec::new(),
                timestamp_ms: Some(2),
                text_fingerprint: fingerprint("retain me"),
                presentation: PersistedPresentation::Plain,
                rendered_text: None,
                rendered_text_fingerprint: None,
            },
        ]);
        let mut cache = render::HistoryRenderCache::default();
        let events = vec![
            FollowerEvent::Conversation(ConversationEvent::User(Message::text(
                "prompt-1",
                "native prompt 1",
                Some(10),
            ))),
            final_event(1),
            FollowerEvent::Conversation(ConversationEvent::User(Message::text(
                "prompt-2",
                "native prompt 2",
                Some(20),
            ))),
            final_event(2),
        ];

        apply_follower_events_with_policy(
            &mut app,
            events,
            &mut cache,
            &runtime,
            CapturePolicy::NewestFinalOnly,
        );

        assert_eq!(
            app.turns
                .iter()
                .map(|turn| turn.prompt.stable_id.as_str())
                .collect::<Vec<_>>(),
            ["prompt-1", "prompt-2", "saved-missing"]
        );
        assert_eq!(
            app.turns[0].final_answer.as_ref().unwrap().stable_id,
            "final-1"
        );
        assert_eq!(
            app.turns[1].final_answer.as_ref().unwrap().stable_id,
            "final-2"
        );
        assert!(app.turns[2].final_answer.is_none());
        assert_eq!(app.replay_insert_at, None);
    }

    #[test]
    fn blocked_key_routes_before_composer_and_preserves_editor_and_history_state() {
        let (runtime, actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            agent_status: AgentStatus::Blocked,
            scroll_from_bottom: 7,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("unchanged draft");
        let before = editor.snapshot();
        let mut sequence = 9;
        let mut cache = render::HistoryRenderCache::default();

        let change = handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(change, super::DraftChange::None);
        assert_eq!(editor.snapshot(), before);
        assert_eq!(app.scroll_from_bottom, 7);
        assert_eq!(sequence, 9);
        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::Interaction(InteractionInput::Key("down"))
        ));
        assert!(actions.try_recv().is_err());
    }

    #[test]
    fn blocked_paste_routes_as_one_text_action_without_composer_work() {
        let (runtime, actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            agent_status: AgentStatus::Blocked,
            ..AppState::default()
        };
        let content = "large body\n".repeat(1_000);

        assert!(handle_blocked_paste(&content, &mut app, &runtime));
        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::Interaction(InteractionInput::Text(text)) if text == content
        ));
        assert!(app.pending_attachments.is_empty());
        assert!(app.draft_attachments.is_empty());
    }

    /// Word editing has to answer both spellings of the option key: a modified
    /// arrow, and the readline escapes that other terminals send instead.
    #[test]
    fn word_hotkeys_edit_the_composer_in_both_spellings() {
        for (delete_left, delete_right, left, right) in [
            (
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Delete, KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Left, KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Right, KeyModifiers::ALT),
            ),
            (
                KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('d'), KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT),
                KeyEvent::new(KeyCode::Char('f'), KeyModifiers::ALT),
            ),
        ] {
            let (runtime, _actions) = runtime::interaction_test_runtime(1);
            let mut app = AppState {
                native_composer: NativeComposerState::Clear,
                ..AppState::default()
            };
            let mut editor = Editor::default();
            editor.insert_paste("alpha beta gamma");
            let mut sequence = 1;
            let mut cache = render::HistoryRenderCache::default();
            let mut press = |key, app: &mut AppState, editor: &mut Editor, cache: &mut _| {
                handle_key(key, app, editor, &runtime, &mut sequence, cache).unwrap()
            };

            assert_eq!(
                press(delete_left, &mut app, &mut editor, &mut cache),
                super::DraftChange::Debounced
            );
            assert_eq!(editor.submission_text(), "alpha beta ");

            assert_eq!(
                press(left, &mut app, &mut editor, &mut cache),
                super::DraftChange::None
            );
            assert_eq!(editor.cursor_byte(), "alpha ".len());

            assert_eq!(
                press(right, &mut app, &mut editor, &mut cache),
                super::DraftChange::None
            );
            assert_eq!(editor.cursor_byte(), "alpha beta".len());

            press(left, &mut app, &mut editor, &mut cache);
            press(delete_right, &mut app, &mut editor, &mut cache);
            assert_eq!(editor.submission_text(), "alpha  ");
        }
    }

    /// macOS keeps the command key for itself, so the readline bindings are
    /// what actually reaches the composer; the super arms only fire where a
    /// terminal is configured to forward that modifier.
    #[test]
    fn line_navigation_answers_readline_and_super_bindings() {
        for (to_start, to_end) in [
            (
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            ),
            (
                KeyEvent::new(KeyCode::Left, KeyModifiers::SUPER),
                KeyEvent::new(KeyCode::Right, KeyModifiers::SUPER),
            ),
            (
                KeyEvent::new(KeyCode::Home, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::End, KeyModifiers::NONE),
            ),
        ] {
            let (runtime, _actions) = runtime::interaction_test_runtime(1);
            let mut app = AppState {
                native_composer: NativeComposerState::Clear,
                ..AppState::default()
            };
            let mut editor = Editor::default();
            editor.insert_paste("first line\nsecond line");
            let mut sequence = 1;
            let mut cache = render::HistoryRenderCache::default();
            let mut press = |key, app: &mut AppState, editor: &mut Editor, cache: &mut _| {
                handle_key(key, app, editor, &runtime, &mut sequence, cache).unwrap()
            };

            press(to_start, &mut app, &mut editor, &mut cache);
            assert_eq!(editor.cursor_byte(), "first line\n".len());
            press(to_end, &mut app, &mut editor, &mut cache);
            assert_eq!(editor.cursor_byte(), "first line\nsecond line".len());
        }
    }

    #[test]
    fn super_arrows_reach_the_ends_of_the_draft() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Clear,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("first line\nsecond line");
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        let mut press = |key, app: &mut AppState, editor: &mut Editor, cache: &mut _| {
            handle_key(key, app, editor, &runtime, &mut sequence, cache).unwrap()
        };

        press(
            KeyEvent::new(KeyCode::Up, KeyModifiers::SUPER),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(editor.cursor_byte(), 0);
        press(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SUPER),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(editor.cursor_byte(), "first line\nsecond line".len());
    }

    /// macOS sends `^U` for command+backspace, so that is the binding that
    /// actually arrives; the super arms cover terminals that forward the
    /// modifier itself.
    #[test]
    fn line_kills_answer_readline_and_super_bindings() {
        for (kill_left, kill_right) in [
            (
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
                KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            ),
            (
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER),
                KeyEvent::new(KeyCode::Delete, KeyModifiers::SUPER),
            ),
        ] {
            let (runtime, _actions) = runtime::interaction_test_runtime(1);
            let mut app = AppState {
                native_composer: NativeComposerState::Clear,
                ..AppState::default()
            };
            let mut editor = Editor::default();
            editor.insert_paste("keep this\ndrop that");
            let mut sequence = 1;
            let mut cache = render::HistoryRenderCache::default();
            let mut press = |key, app: &mut AppState, editor: &mut Editor, cache: &mut _| {
                handle_key(key, app, editor, &runtime, &mut sequence, cache).unwrap()
            };

            assert_eq!(
                press(kill_left, &mut app, &mut editor, &mut cache),
                super::DraftChange::Debounced
            );
            assert_eq!(editor.submission_text(), "keep this\n");

            editor.insert_paste("tail text");
            editor.move_home();
            assert_eq!(
                press(kill_right, &mut app, &mut editor, &mut cache),
                super::DraftChange::Debounced
            );
            assert_eq!(editor.submission_text(), "keep this\n");
        }
    }

    const OCCUPIED_CODEX: &str = concat!(
        "• answer\n",
        "────────\n",
        "› half written prompt\n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    );
    const EMPTY_CODEX: &str = concat!(
        "• answer\n",
        "────────\n",
        "› \n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    );
    const QUEUED_CODEX: &str = concat!(
        "• Working (2s • esc to interrupt)\n",
        "› half written steer\n",
        "tab to queue message                  55% context left",
    );

    /// Opening the overlay over a half-written prompt should not mean switching
    /// back to finish it.
    #[test]
    fn adopting_a_native_draft_moves_the_text_and_clears_the_source() {
        let cleared = std::cell::Cell::new(0);
        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || {
                Ok(if cleared.get() == 0 {
                    OCCUPIED_CODEX.to_owned()
                } else {
                    EMPTY_CODEX.to_owned()
                })
            },
            |_| {
                cleared.set(cleared.get() + 1);
                Ok(())
            },
            8,
            std::time::Duration::ZERO,
        )
        .unwrap()
        .expect("an occupied composer must be adopted");

        assert_eq!(adopted.text, "half written prompt");
        assert!(adopted.markers.is_empty());
        assert!(adopted.cleared);
        assert!(cleared.get() > 0, "the composer must have been cleared");
    }

    #[test]
    fn adopting_an_active_turn_draft_moves_it_into_the_overlay() {
        let cleared = std::cell::Cell::new(0);
        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || {
                Ok(if cleared.get() == 0 {
                    QUEUED_CODEX.to_owned()
                } else {
                    EMPTY_CODEX.to_owned()
                })
            },
            |_| {
                cleared.set(cleared.get() + 1);
                Ok(())
            },
            8,
            std::time::Duration::ZERO,
        )
        .unwrap()
        .expect("a queued steer must be adopted");

        assert_eq!(adopted.text, "half written steer");
        assert!(adopted.markers.is_empty());
        assert!(adopted.cleared);
        assert!(cleared.get() > 0, "the native copy must be cleared");
    }

    #[test]
    fn an_empty_native_composer_is_left_alone() {
        let cleared = std::cell::Cell::new(0);
        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || Ok(EMPTY_CODEX.to_owned()),
            |_| {
                cleared.set(cleared.get() + 1);
                Ok(())
            },
            8,
            std::time::Duration::ZERO,
        )
        .unwrap();

        assert_eq!(adopted, None);
        assert_eq!(cleared.get(), 0, "nothing may be sent to an empty composer");
    }

    /// A draft is never lost to a half-finished takeover: the text comes back
    /// even when the source could not be cleared, and the caller says so.
    #[test]
    fn a_draft_that_cannot_be_cleared_is_still_adopted() {
        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || Ok(OCCUPIED_CODEX.to_owned()),
            |_| Ok(()),
            3,
            std::time::Duration::ZERO,
        )
        .unwrap()
        .expect("the text must survive a failed takeover");

        assert_eq!(adopted.text, "half written prompt");
        assert!(!adopted.cleared);
    }

    const IMAGE_AND_TEXT: &str = concat!(
        "• answer\n",
        "────────\n",
        "› [Image #1] describe it\n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    );
    const IMAGE_ONLY: &str = concat!(
        "• answer\n",
        "────────\n",
        "› [Image #1]\n",
        "gpt-5.6-sol xhigh · /repo · weekly 75% left",
    );

    /// An image cannot be carried, but the text beside it can: the marker stays
    /// where it is and only the characters of the prompt are removed, one at a
    /// time, so an overshoot can never eat the marker.
    #[test]
    fn text_beside_an_image_is_adopted_without_disturbing_the_image() {
        let keys: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || {
                Ok(if keys.borrow().is_empty() {
                    IMAGE_AND_TEXT.to_owned()
                } else {
                    IMAGE_ONLY.to_owned()
                })
            },
            |pressed| {
                keys.borrow_mut()
                    .extend(pressed.iter().map(|key| (*key).to_owned()));
                Ok(())
            },
            8,
            std::time::Duration::ZERO,
        )
        .unwrap()
        .expect("text beside an image must be adopted");

        assert_eq!(adopted.text, "describe it");
        assert_eq!(adopted.markers, [1]);
        assert!(adopted.cleared);
        let pressed = keys.borrow();
        assert_eq!(pressed[0], "ctrl+e");
        assert!(
            !pressed.iter().any(|key| key == "ctrl+u"),
            "a line kill would take the image marker with it"
        );
        assert_eq!(
            pressed.iter().filter(|key| *key == "backspace").count(),
            "describe it".chars().count(),
            "exactly the characters of the text, never one more"
        );
    }

    /// A bare image is adopted without touching the composer: there is nothing
    /// to lift out, and the overlay only has to learn the image is there.
    #[test]
    fn a_bare_image_is_adopted_without_pressing_anything() {
        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || Ok(IMAGE_ONLY.to_owned()),
            |_| panic!("a composer with nothing to lift must not be touched"),
            8,
            std::time::Duration::ZERO,
        )
        .unwrap()
        .expect("a bare image must be adopted");

        assert_eq!(adopted.text, "");
        assert_eq!(adopted.markers, [1]);
        assert!(adopted.cleared);
    }

    /// The count and the adoption have to come from the same reading: a redraw
    /// between two of them can retire the marker just adopted, and the overlay
    /// answers a count it disagrees with by guarding its own input.
    #[test]
    fn the_count_and_the_adoption_come_from_one_reading() {
        let reads = std::cell::Cell::new(0);
        let view = super::inspect_native_composer(
            AgentKind::Codex,
            true,
            || {
                reads.set(reads.get() + 1);
                Ok(IMAGE_ONLY.to_owned())
            },
            |_| Ok(()),
            8,
            std::time::Duration::ZERO,
        )
        .unwrap();

        assert_eq!(view.attachments.as_deref(), Some([1].as_slice()));
        assert_eq!(view.adopted.expect("a bare image is adopted").markers, [1]);
        assert_eq!(reads.get(), 1);
    }

    /// A draft of its own is measured against the pane but never overwritten by
    /// it, or a saved marker would be counted twice.
    #[test]
    fn a_draft_of_its_own_is_measured_but_not_adopted() {
        let view = super::inspect_native_composer(
            AgentKind::Codex,
            false,
            || Ok(IMAGE_ONLY.to_owned()),
            |_| panic!("a draft of its own must not be lifted from the pane"),
            8,
            std::time::Duration::ZERO,
        )
        .unwrap();

        assert_eq!(view.attachments.as_deref(), Some([1].as_slice()));
        assert!(view.adopted.is_none());
    }

    /// A wrapped draft carries newlines the buffer does not have, so counting
    /// deletions from it would overshoot into the marker.
    #[test]
    fn a_wrapped_draft_beside_an_image_is_left_alone() {
        let wrapped = concat!(
            "• answer\n",
            "────────\n",
            "› [Image #1] describe it\n",
            "  and then some more\n",
            "gpt-5.6-sol xhigh · /repo · weekly 75% left",
        );

        let adopted = super::adopt_native_draft(
            AgentKind::Codex,
            || Ok(wrapped.to_owned()),
            |_| panic!("a draft that cannot be measured must not be touched"),
            8,
            std::time::Duration::ZERO,
        )
        .unwrap();

        assert_eq!(adopted, None);
    }

    /// The picture lives in the native composer, so backspace on an image asks
    /// the pane to lose it and waits: dropping it here first would leave the
    /// two sides disagreeing about what the prompt carries.
    #[test]
    fn backspace_on_an_image_asks_the_pane_before_anything_is_dropped() {
        let (runtime, actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::OwnedAttachments(1),
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_attachment(Attachment {
            id: "native-image-5".into(),
            display: "Image #5".into(),
            native_path: None,
        });
        app.draft_attachments = editor.attachments();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        // Step back onto the image: past its gap, then onto the marker, which
        // is where it shows as marked and so where backspace means it.
        editor.move_left();
        editor.move_left();
        let change = handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(change, super::DraftChange::None);
        assert_eq!(editor.attachments().len(), 1, "nothing is dropped yet");
        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::RemoveAttachment { marker: 5, .. }
        ));
    }

    /// The pane numbers an image when it takes it, and that number is what the
    /// overlay must later name when asking for it back — a number invented here
    /// would point at a different picture.
    #[test]
    fn a_pasted_image_takes_the_number_the_pane_gave_it() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let original = AgentIdentity {
            pane_id: "w1:p1".into(),
            kind: AgentKind::Codex,
            session_id: "session-1".into(),
            cwd: PathBuf::from("/repo"),
            status: AgentStatus::Idle,
        };
        let mut app = AppState::default();
        let mut editor = Editor::default();
        let mut cache = render::HistoryRenderCache::default();

        apply_runtime_event(
            runtime::RuntimeEvent::ImageForwarded {
                attachment: Attachment {
                    id: "local-image-1".into(),
                    display: "Image #1".into(),
                    native_path: None,
                },
                result: Ok(7),
            },
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );

        assert_eq!(editor.display_text(), "[Image #7] ");
        assert_eq!(app.draft_attachments.len(), 1);
        assert_eq!(app.draft_attachments[0].display, "Image #7");
    }

    /// Standing just past an image there is a space on the screen between the
    /// cursor and the picture, and that space is what backspace takes first.
    /// The picture goes on the press after — as it does in the native composer,
    /// and as anyone looking at the space would expect.
    #[test]
    fn backspace_takes_the_space_beside_an_image_before_the_image() {
        let (runtime, actions) = runtime::interaction_test_runtime(2);
        let mut app = AppState {
            native_composer: NativeComposerState::OwnedAttachments(1),
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_attachment(Attachment {
            id: "native-image-5".into(),
            display: "Image #5".into(),
            native_path: None,
        });
        app.draft_attachments = editor.attachments();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        // The cursor sits past the gap that follows the image.
        let change = handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(change, super::DraftChange::Debounced, "the space is ours");
        assert_eq!(editor.display_text(), "[Image #5]", "so it simply goes");
        assert!(
            actions.try_recv().is_err(),
            "and the pane is not troubled for it"
        );

        let change = handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(change, super::DraftChange::None, "the pane is asked first");
        assert_eq!(editor.attachments().len(), 1, "nothing is dropped yet");
        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::RemoveAttachment { marker: 5, .. }
        ));
    }

    /// Clearing back to the start of the line takes the pictures in the way
    /// too. They are not ours to drop, so each is asked for and waited on, and
    /// the clear carries on as the answers come back.
    #[test]
    fn clearing_a_line_asks_for_the_pictures_in_the_way() {
        let (runtime, actions) = runtime::interaction_test_runtime(4);
        let mut app = AppState {
            native_composer: NativeComposerState::OwnedAttachments(2),
            composer_width: 80,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        for marker in ["Image #5", "Image #6"] {
            editor.insert_attachment(Attachment {
                id: format!("native-image-{}", &marker["Image #".len()..]),
                display: marker.into(),
                native_path: None,
            });
        }
        editor.insert_paste("describe these");
        app.draft_attachments = editor.attachments();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(
            editor.display_text(),
            "[Image #5] [Image #6]",
            "the text goes at once, the pictures wait on the pane"
        );
        assert!(
            app.clearing_line_to.is_some(),
            "and the clear is still going"
        );
        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::RemoveAttachment { marker: 6, .. }
        ));
        assert!(
            actions.try_recv().is_err(),
            "one at a time, as the pane answers them"
        );
    }

    /// A paragraph that has wrapped is several lines on the screen, and the one
    /// a clear takes is the one the cursor is on. Taking the whole paragraph
    /// would throw away lines the person can see they are not standing in.
    #[test]
    fn clearing_a_wrapped_paragraph_takes_only_the_line_the_cursor_is_on() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            composer_width: 12,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("one two three four");
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::SUPER),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(
            editor.display_text(),
            "one two ",
            "the row the cursor was on goes, the one above it stays"
        );
    }

    /// Backspace held down while the pane is still taking the picture away
    /// used to send the same removal again. The pane answers these in order, so
    /// the second one named an image that had just gone and came back as an
    /// error over a removal that had worked.
    #[test]
    fn a_second_removal_is_not_asked_for_while_the_first_is_running() {
        let (runtime, actions) = runtime::interaction_test_runtime(4);
        let mut app = AppState {
            native_composer: NativeComposerState::OwnedAttachments(1),
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_attachment(Attachment {
            id: "native-image-5".into(),
            display: "Image #5".into(),
            native_path: None,
        });
        app.draft_attachments = editor.attachments();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        for _ in 0..3 {
            handle_key(
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
                &mut app,
                &mut editor,
                &runtime,
                &mut sequence,
                &mut cache,
            )
            .unwrap();
        }

        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::RemoveAttachment { marker: 5, .. }
        ));
        assert!(
            actions.try_recv().is_err(),
            "the presses that land in the wait are let go of"
        );
        assert_eq!(editor.attachments().len(), 1, "and nothing is dropped yet");
        assert!(
            app.background_error.is_none(),
            "so no failure is reported over a removal that is still running"
        );
    }

    #[test]
    fn an_image_is_dropped_only_once_the_pane_has_lost_it() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let original = AgentIdentity {
            pane_id: "w1:p1".into(),
            kind: AgentKind::Codex,
            session_id: "session-1".into(),
            cwd: PathBuf::from("/repo"),
            status: AgentStatus::Idle,
        };
        let mut app = AppState::default();
        let mut editor = Editor::default();
        editor.insert_attachment(Attachment {
            id: "native-image-5".into(),
            display: "Image #5".into(),
            native_path: None,
        });
        editor.insert_paste("describe it");
        app.draft_attachments = editor.attachments();
        let mut cache = render::HistoryRenderCache::default();

        apply_runtime_event(
            runtime::RuntimeEvent::AttachmentRemoved {
                id: "native-image-5".into(),
                result: Err("could not reach the image".into()),
            },
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );
        assert_eq!(editor.attachments().len(), 1, "a refusal changes nothing");
        assert!(app.background_error.is_some());

        apply_runtime_event(
            runtime::RuntimeEvent::AttachmentRemoved {
                id: "native-image-5".into(),
                result: Ok(()),
            },
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );

        assert!(editor.attachments().is_empty());
        assert!(app.draft_attachments.is_empty());
        assert_eq!(
            editor.submission_text().trim_start(),
            "describe it",
            "the gap the marker stood beside is ordinary text and stays"
        );
    }

    #[test]
    fn word_hotkeys_stay_blocked_while_the_composer_is_guarded() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Occupied,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("alpha beta");
        let before = editor.snapshot();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        handle_key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert_eq!(editor.snapshot(), before);
    }

    #[test]
    fn occupied_and_unknown_composers_block_ordinary_editor_mutation_and_submit() {
        for native_composer in [NativeComposerState::Occupied, NativeComposerState::Unknown] {
            let (runtime, actions) = runtime::interaction_test_runtime(1);
            let mut app = AppState {
                native_composer,
                ..AppState::default()
            };
            let mut editor = Editor::default();
            editor.insert_paste("preserved draft");
            let before = editor.snapshot();
            let mut sequence = 1;
            let mut cache = render::HistoryRenderCache::default();

            let change = handle_key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
                &mut app,
                &mut editor,
                &runtime,
                &mut sequence,
                &mut cache,
            )
            .unwrap();

            assert_eq!(change, super::DraftChange::None);
            assert_eq!(editor.snapshot(), before);
            assert!(actions.try_recv().is_err());

            handle_key(
                KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
                &mut app,
                &mut editor,
                &runtime,
                &mut sequence,
                &mut cache,
            )
            .unwrap();
            handle_key(
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &mut app,
                &mut editor,
                &runtime,
                &mut sequence,
                &mut cache,
            )
            .unwrap();

            assert_eq!(editor.snapshot(), before);
            assert!(app.pending_attachments.is_empty());
            assert!(actions.try_recv().is_err());
        }
    }

    /// The wheel is delivered as arrow keys (alternate scroll), so the arrows
    /// have to drive the history when the composer holds nothing to move a
    /// cursor through — and must not steal the cursor when it does.
    #[test]
    fn arrows_scroll_the_history_only_when_the_composer_has_no_text() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Clear,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        for index in 0..8 {
            app.apply(crate::app::AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
        }
        cache.viewport_rows(&app, 40, 3);

        let mut press = |key, app: &mut AppState, editor: &mut Editor, cache: &mut _| {
            handle_key(
                KeyEvent::new(key, KeyModifiers::NONE),
                app,
                editor,
                &runtime,
                &mut sequence,
                cache,
            )
            .unwrap()
        };

        press(KeyCode::Up, &mut app, &mut editor, &mut cache);
        assert!(
            app.scroll_from_bottom > 0,
            "an empty composer must scroll the history"
        );
        let scrolled = app.scroll_from_bottom;
        press(KeyCode::Down, &mut app, &mut editor, &mut cache);
        assert!(app.scroll_from_bottom < scrolled);

        editor.insert_paste("draft line one\ndraft line two");
        let offset = app.scroll_from_bottom;
        let cursor = editor.display_cursor_byte();
        press(KeyCode::Up, &mut app, &mut editor, &mut cache);

        assert_eq!(
            app.scroll_from_bottom, offset,
            "a composer with text must keep the arrows for the cursor"
        );
        assert_ne!(editor.display_cursor_byte(), cursor);
    }

    #[test]
    fn arrows_scroll_the_history_while_the_composer_is_guarded() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Occupied,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("preserved draft");
        let before = editor.snapshot();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        for index in 0..8 {
            app.apply(crate::app::AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
        }
        cache.viewport_rows(&app, 40, 3);

        handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(app.scroll_from_bottom > 0);
        assert_eq!(editor.snapshot(), before);
    }

    #[test]
    fn page_navigation_remains_available_while_composer_is_guarded() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Occupied,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        for index in 0..8 {
            app.apply(crate::app::AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
        }
        let _ = render::render_to_string(&app, &editor, 40, 8);
        cache.viewport_rows(&app, 40, 3);

        handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(app.scroll_from_bottom > 0);
    }

    /// A draft must never cost the reader the history: the shift arrows keep
    /// scrolling while the bare arrows stay with the cursor.
    #[test]
    fn shift_arrows_scroll_the_history_while_the_composer_holds_a_draft() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Clear,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("draft line one\ndraft line two");
        let cursor = editor.display_cursor_byte();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        for index in 0..8 {
            app.apply(crate::app::AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
        }
        cache.viewport_rows(&app, 40, 3);

        handle_key(
            KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(app.scroll_from_bottom > 0);
        assert_eq!(editor.display_cursor_byte(), cursor);

        let scrolled = app.scroll_from_bottom;
        handle_key(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(app.scroll_from_bottom < scrolled);
        assert_eq!(editor.display_cursor_byte(), cursor);
    }

    /// A page is the view rather than a fixed count, and both ends of the
    /// history are one press away.
    #[test]
    fn page_keys_move_by_the_view_and_shift_home_end_reach_both_ends() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::Clear,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        for index in 0..20 {
            app.apply(crate::app::AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
        }
        cache.viewport_rows(&app, 40, 10);

        let mut press = |key, app: &mut AppState, editor: &mut Editor, cache: &mut _| {
            handle_key(key, app, editor, &runtime, &mut sequence, cache).unwrap()
        };

        press(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(app.scroll_from_bottom, cache.page_rows());
        assert_eq!(
            cache.page_rows(),
            8,
            "a page keeps two rows of the old view"
        );

        press(
            KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(app.scroll_from_bottom, 0);

        press(
            KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(app.scroll_from_bottom, cache.maximum_offset());
        assert!(app.scroll_from_bottom > 0);

        press(
            KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(app.scroll_from_bottom, 0);

        // The same two ends, spelled for a keyboard without home and end.
        press(
            KeyEvent::new(KeyCode::Up, KeyModifiers::SHIFT | KeyModifiers::ALT),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(app.scroll_from_bottom, cache.maximum_offset());

        press(
            KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT | KeyModifiers::ALT),
            &mut app,
            &mut editor,
            &mut cache,
        );
        assert_eq!(app.scroll_from_bottom, 0);
    }

    /// A blocking question owns the keys it can use. These are not among them:
    /// they scroll here and are never forwarded to the native pane.
    #[test]
    fn history_scroll_keys_work_while_a_native_question_is_blocking() {
        let (runtime, actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            agent_status: AgentStatus::Blocked,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();
        for index in 0..8 {
            app.apply(crate::app::AppEvent::NativeUser(Message::text(
                format!("u{index}"),
                format!("prompt {index}"),
                Some(index),
            )));
        }
        cache.viewport_rows(&app, 40, 3);

        handle_key(
            KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(app.scroll_from_bottom > 0);
        assert!(actions.try_recv().is_err());
    }

    #[test]
    fn guarded_composer_blocks_text_and_staged_image_paste() {
        let (runtime, actions) = runtime::interaction_test_runtime(2);
        let mut app = AppState {
            native_composer: NativeComposerState::Occupied,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("preserved draft");
        let before = editor.snapshot();
        let mut sequence = 1;

        assert_eq!(
            handle_ordinary_paste(
                "additional text",
                &mut app,
                &mut editor,
                &runtime,
                &mut sequence,
            ),
            super::DraftChange::None
        );
        assert_eq!(
            handle_ordinary_paste(
                "/private/tmp/herdr-paste-does-not-exist/image.png",
                &mut app,
                &mut editor,
                &runtime,
                &mut sequence,
            ),
            super::DraftChange::None
        );

        assert_eq!(editor.snapshot(), before);
        assert!(app.pending_attachments.is_empty());
        assert!(actions.try_recv().is_err());
        assert_eq!(sequence, 1);
    }

    #[test]
    fn connection_failure_blocks_input_even_if_composer_snapshot_was_clear() {
        let app = AppState {
            input_enabled: false,
            native_composer: NativeComposerState::Clear,
            ..AppState::default()
        };

        assert!(!super::ordinary_input_allowed(&app));
    }

    #[test]
    fn safe_observation_reenables_preserved_editor_draft() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let original = AgentIdentity {
            pane_id: "w1:p1".into(),
            kind: AgentKind::Codex,
            session_id: "session-1".into(),
            cwd: PathBuf::from("/repo"),
            status: AgentStatus::Done,
        };
        let mut app = AppState {
            native_composer: NativeComposerState::Occupied,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("preserved draft");
        let before = editor.snapshot();
        let mut cache = render::HistoryRenderCache::default();

        apply_runtime_event(
            runtime::RuntimeEvent::Observation(Ok(runtime::SourceObservation {
                identity: original.clone(),
                status_text: "gpt-5.6-sol · /repo · weekly 75% left".into(),
                native_composer: NativeComposerState::Clear,
                blocked_surface: None,
            })),
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );

        assert_eq!(app.native_composer, NativeComposerState::Clear);
        assert_eq!(editor.snapshot(), before);
    }

    #[test]
    fn asynchronous_preflight_failure_restores_exact_draft_once() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let original = AgentIdentity {
            pane_id: "w1:p1".into(),
            kind: AgentKind::Codex,
            session_id: "session-1".into(),
            cwd: PathBuf::from("/repo"),
            status: AgentStatus::Done,
        };
        let mut app = AppState::default();
        let mut editor = Editor::default();
        editor.insert_paste(&"private\n".repeat(1_000));
        let submission = editor.take_editor_submission();
        let recovery = submission.recovery.clone();
        app.apply(crate::app::AppEvent::PromptSubmitted {
            local_id: "local-1".into(),
            submission,
            attachments: Vec::new(),
            at_ms: 1,
        });
        let mut cache = render::HistoryRenderCache::default();

        let failed = runtime::RuntimeEvent::Submitted {
            local_id: "local-1".into(),
            result: Err("native composer contains unsent input".into()),
        };
        apply_runtime_event(
            failed,
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );

        assert_eq!(app.draft, recovery);
        assert_eq!(editor.snapshot(), recovery);
        assert_eq!(app.turns.len(), 1);
        assert!(matches!(
            app.turns[0].delivery,
            crate::model::Delivery::Failed { .. }
        ));

        apply_runtime_event(
            runtime::RuntimeEvent::Submitted {
                local_id: "local-1".into(),
                result: Err("native composer contains unsent input".into()),
            },
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );
        assert_eq!(app.draft, recovery);
        assert_eq!(app.turns.len(), 1);
    }

    #[test]
    fn escape_interrupts_working_agent_even_when_composer_is_unknown() {
        let (runtime, actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            agent_status: AgentStatus::Working,
            native_composer: NativeComposerState::Unknown,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        handle_key(
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::Interrupt
        ));
    }

    #[test]
    fn submit_captures_confirmed_attachment_count_before_optimistic_clear() {
        let (runtime, actions) = runtime::interaction_test_runtime(1);
        let mut app = AppState {
            native_composer: NativeComposerState::OwnedAttachments(1),
            draft_attachments: vec![Attachment {
                id: "image-1".into(),
                display: "screen.png".into(),
                native_path: None,
            }],
            ..AppState::default()
        };
        let mut editor = Editor::default();
        editor.insert_paste("describe it");
        let mut sequence = 1;
        let mut cache = render::HistoryRenderCache::default();

        handle_key(
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            &mut app,
            &mut editor,
            &runtime,
            &mut sequence,
            &mut cache,
        )
        .unwrap();

        assert!(matches!(
            actions.try_recv().unwrap(),
            runtime::ActionCommand::Submit {
                expected_attachments: 1,
                ..
            }
        ));
        assert!(app.draft_attachments.is_empty());
    }

    #[test]
    fn failed_observation_replaces_a_stale_clear_composer_with_unknown() {
        let (runtime, _actions) = runtime::interaction_test_runtime(1);
        let original = AgentIdentity {
            pane_id: "w1:p1".into(),
            kind: AgentKind::Codex,
            session_id: "session-1".into(),
            cwd: PathBuf::from("/repo"),
            status: AgentStatus::Done,
        };
        let mut app = AppState {
            native_composer: NativeComposerState::Clear,
            ..AppState::default()
        };
        let mut editor = Editor::default();
        let mut cache = render::HistoryRenderCache::default();

        apply_runtime_event(
            runtime::RuntimeEvent::Observation(Err("screen unavailable".into())),
            &original,
            &mut app,
            &mut editor,
            &mut cache,
            &runtime,
        );

        assert_eq!(app.native_composer, NativeComposerState::Unknown);
        assert!(!app.input_enabled);
    }
}
