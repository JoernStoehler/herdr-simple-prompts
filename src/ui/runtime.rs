use crate::agent::follower::{FollowerEvent, TranscriptFollower};
use crate::agent::{AgentIdentity, AgentKind, AgentStatus};
use crate::ansi::{extract_native_final, sanitize_ansi};
use crate::composer::{NativeComposerState, classify_native_composer};
use crate::herdr::HerdrClient;
use crate::markdown::style_markdown;
use crate::model::Attachment;
use crate::native_chrome::is_known_footer;
use crate::paste::fingerprint;
use crate::style::MessagePresentation;
use crate::style::StyledText;
use crate::transport::AgentTransport;
use crate::{AppError, AppResult};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use super::interaction::InteractionInput;

const EVENT_QUEUE_CAPACITY: usize = 64;
const ACTION_QUEUE_CAPACITY: usize = 16;
const CAPTURE_QUEUE_CAPACITY: usize = 8;
const CAPTURE_ATTEMPTS: usize = 8;
const CAPTURE_LINES: u32 = 240;
const CAPTURE_RETRY_DELAY: Duration = Duration::from_millis(75);
const LIFECYCLE_WAIT: Duration = Duration::from_secs(1);
const LIFECYCLE_RETRY_INITIAL: Duration = Duration::from_millis(200);
const LIFECYCLE_RETRY_MAX: Duration = Duration::from_secs(1);
const LIFECYCLE_STOP_POLL: Duration = Duration::from_millis(50);
const TRANSCRIPT_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug)]
pub(super) enum ActionCommand {
    Submit {
        local_id: String,
        text: String,
        expected_attachments: usize,
    },
    Interrupt,
    Interaction(InteractionInput),
    LocalImage {
        attachment: Attachment,
    },
    StagedImage {
        attachment: Attachment,
        path: PathBuf,
    },
    RemoveAttachment {
        id: String,
        marker: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceObservation {
    pub identity: AgentIdentity,
    pub status_text: String,
    pub native_composer: NativeComposerState,
    pub blocked_surface: Option<Result<StyledText, String>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CaptureCommand {
    pub(super) stable_id: String,
    pub(super) canonical_text: String,
}

#[derive(Debug)]
pub enum RuntimeEvent {
    Transcript(Vec<FollowerEvent>),
    TranscriptError(String),
    Observation(Result<SourceObservation, String>),
    Submitted {
        local_id: String,
        result: Result<(), String>,
    },
    Interrupted(Result<(), String>),
    InteractionForwarded(Result<(), String>),
    ImageForwarded {
        attachment: Attachment,
        /// The number the pane gave the image, so the overlay can call it what
        /// the pane calls it.
        result: Result<usize, String>,
    },
    AttachmentRemoved {
        id: String,
        result: Result<(), String>,
    },
    FinalPresentation {
        stable_id: String,
        text_fingerprint: u64,
        presentation: MessagePresentation,
    },
    CaptureDiagnostic(String),
    SourcePaneClosed,
}

pub struct UiRuntime {
    action_tx: SyncSender<ActionCommand>,
    capture_tx: SyncSender<CaptureCommand>,
    events: Receiver<RuntimeEvent>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}

impl UiRuntime {
    pub fn spawn(
        socket: &Path,
        identity: AgentIdentity,
        follower: Option<TranscriptFollower>,
        open_transcript: OpenTranscript,
    ) -> AppResult<Self> {
        let (event_tx, events) = sync_channel(EVENT_QUEUE_CAPACITY);
        let (action_tx, action_rx) = sync_channel(ACTION_QUEUE_CAPACITY);
        let (capture_tx, capture_rx) = sync_channel(CAPTURE_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let follower_active = Arc::new(AtomicBool::new(follower_is_active(identity.status)));

        let observer_transport = AgentTransport::new(
            HerdrClient::connect(socket).map_err(|error| AppError::new("ui", error.to_string()))?,
            identity.clone(),
        );
        let action_transport = AgentTransport::new(
            HerdrClient::connect(socket).map_err(|error| AppError::new("ui", error.to_string()))?,
            identity.clone(),
        );
        let capture_transport = AgentTransport::new(
            HerdrClient::connect(socket).map_err(|error| AppError::new("ui", error.to_string()))?,
            identity.clone(),
        );
        let lifecycle_client = HerdrClient::connect(socket)
            .map_err(|error| AppError::new("ui", error.to_string()))?
            .with_timeout(Duration::from_millis(1_100));

        let threads = vec![
            spawn_observer(
                Arc::clone(&stop),
                Arc::clone(&follower_active),
                event_tx.clone(),
                observer_transport,
            ),
            spawn_follower(
                Arc::clone(&stop),
                follower_active,
                event_tx.clone(),
                follower,
                open_transcript,
            ),
            spawn_actions(
                Arc::clone(&stop),
                event_tx.clone(),
                action_rx,
                action_transport,
            ),
            spawn_capture_worker(
                Arc::clone(&stop),
                event_tx.clone(),
                capture_rx,
                capture_transport,
                identity.kind,
            ),
            spawn_lifecycle_worker(
                Arc::clone(&stop),
                event_tx,
                lifecycle_client,
                identity.pane_id,
            ),
        ];

        Ok(Self {
            action_tx,
            capture_tx,
            events,
            stop,
            threads,
        })
    }

    pub fn try_recv(&self) -> Option<RuntimeEvent> {
        self.events.try_recv().ok()
    }

    pub fn submit(
        &self,
        local_id: String,
        text: String,
        expected_attachments: usize,
    ) -> AppResult<()> {
        self.send_action(ActionCommand::Submit {
            local_id,
            text,
            expected_attachments,
        })
    }

    pub fn interrupt(&self) -> AppResult<()> {
        self.send_action(ActionCommand::Interrupt)
    }

    pub fn forward_interaction(&self, input: InteractionInput) -> AppResult<()> {
        self.send_action(ActionCommand::Interaction(input))
    }

    pub fn forward_local_image(&self, attachment: Attachment) -> AppResult<()> {
        self.send_action(ActionCommand::LocalImage { attachment })
    }

    pub fn forward_staged_image(&self, attachment: Attachment, path: PathBuf) -> AppResult<()> {
        self.send_action(ActionCommand::StagedImage { attachment, path })
    }

    pub fn remove_attachment(&self, id: String, marker: usize) -> AppResult<()> {
        self.send_action(ActionCommand::RemoveAttachment { id, marker })
    }

    /// Queues a final answer for native style capture.
    ///
    /// Returns `Ok(false)` when the queue is saturated. Capture only upgrades
    /// the colours of an answer that already renders through the markdown
    /// fallback, so backpressure is ordinary and must not reach the user as an
    /// error line over a perfectly good answer.
    pub fn capture_final(&self, stable_id: String, canonical_text: String) -> AppResult<bool> {
        match self.capture_tx.try_send(CaptureCommand {
            stable_id,
            canonical_text,
        }) {
            Ok(()) => Ok(true),
            Err(TrySendError::Full(_)) => Ok(false),
            Err(TrySendError::Disconnected(_)) => Err(AppError::new(
                "ui",
                "final style capture worker has stopped",
            )),
        }
    }

    fn send_action(&self, command: ActionCommand) -> AppResult<()> {
        self.action_tx
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => AppError::new("ui", "agent action queue is full"),
                TrySendError::Disconnected(_) => {
                    AppError::new("ui", "agent action worker has stopped")
                }
            })
    }
}

fn spawn_lifecycle_worker(
    stop: Arc<AtomicBool>,
    events: SyncSender<RuntimeEvent>,
    client: HerdrClient,
    source_pane: String,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut retry_delay = LIFECYCLE_RETRY_INITIAL;
        while !stop.load(Ordering::Acquire) {
            match client.wait_for_pane_closed(&source_pane, LIFECYCLE_WAIT) {
                Ok(true) => {
                    let _ = events.send(RuntimeEvent::SourcePaneClosed);
                    break;
                }
                Ok(false) => {
                    if source_pane_is_gone(&client, &source_pane) {
                        let _ = events.send(RuntimeEvent::SourcePaneClosed);
                        break;
                    }
                    retry_delay = LIFECYCLE_RETRY_INITIAL;
                }
                Err(_) => {
                    if source_pane_is_gone(&client, &source_pane) {
                        let _ = events.send(RuntimeEvent::SourcePaneClosed);
                        break;
                    }
                    sleep_while_running(&stop, retry_delay);
                    retry_delay = next_lifecycle_retry(retry_delay);
                }
            }
        }
    })
}

fn source_pane_is_gone(client: &HerdrClient, source_pane: &str) -> bool {
    matches!(
        client.pane_get(source_pane),
        Err(error) if error.is_pane_not_found()
    )
}

fn next_lifecycle_retry(current: Duration) -> Duration {
    current.saturating_mul(2).min(LIFECYCLE_RETRY_MAX)
}

fn sleep_while_running(stop: &AtomicBool, duration: Duration) {
    let mut remaining = duration;
    while !remaining.is_zero() && !stop.load(Ordering::Acquire) {
        let step = remaining.min(LIFECYCLE_STOP_POLL);
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
}

impl Drop for UiRuntime {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let (_replacement_tx, replacement_events) = sync_channel(1);
        let events = std::mem::replace(&mut self.events, replacement_events);
        drop(events);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
pub(super) fn capture_test_runtime(capacity: usize) -> (UiRuntime, Receiver<CaptureCommand>) {
    let (action_tx, _action_rx) = sync_channel(1);
    let (capture_tx, capture_rx) = sync_channel(capacity);
    let (_event_tx, events) = sync_channel(1);
    (
        UiRuntime {
            action_tx,
            capture_tx,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        },
        capture_rx,
    )
}

#[cfg(test)]
pub(super) fn interaction_test_runtime(capacity: usize) -> (UiRuntime, Receiver<ActionCommand>) {
    let (action_tx, action_rx) = sync_channel(capacity);
    let (capture_tx, _capture_rx) = sync_channel(1);
    let (_event_tx, events) = sync_channel(1);
    (
        UiRuntime {
            action_tx,
            capture_tx,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        },
        action_rx,
    )
}

fn spawn_observer(
    stop: Arc<AtomicBool>,
    follower_active: Arc<AtomicBool>,
    events: SyncSender<RuntimeEvent>,
    transport: AgentTransport,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let observation = match transport.refresh_identity() {
                Ok(identity) => {
                    follower_active.store(follower_is_active(identity.status), Ordering::Release);
                    complete_observation(identity, || transport.read_visible_source_ansi(200))
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = events.try_send(RuntimeEvent::Observation(observation));
            thread::sleep(Duration::from_millis(200));
        }
    })
}

fn follower_is_active(status: AgentStatus) -> bool {
    status.keeps_turn_open()
}

fn complete_observation(
    identity: AgentIdentity,
    read_ansi: impl FnOnce() -> AppResult<String>,
) -> Result<SourceObservation, String> {
    let ansi = read_ansi().map_err(|error| error.to_string())?;
    let surface = sanitize_ansi(&ansi);
    let status_text = observation_status_text(identity.kind, &surface.text).to_owned();
    let native_composer = classify_native_composer(identity.kind, &surface);
    let blocked_surface = (identity.status == AgentStatus::Blocked).then_some(Ok(surface));
    Ok(SourceObservation {
        identity,
        status_text,
        native_composer,
        blocked_surface,
    })
}

fn observation_status_text(kind: AgentKind, text: &str) -> &str {
    let mut nonempty = text.lines().rev().filter(|line| !line.trim().is_empty());
    match kind {
        AgentKind::Codex => nonempty
            .find(|line| is_known_footer(line))
            .or_else(|| text.lines().rev().find(|line| !line.trim().is_empty())),
        AgentKind::Claude => nonempty.next(),
    }
    .unwrap_or_default()
}

/// Opens the transcript, once there is one to open.
pub(super) type OpenTranscript = Box<dyn FnMut() -> AppResult<TranscriptFollower> + Send>;

fn spawn_follower(
    stop: Arc<AtomicBool>,
    follower_active: Arc<AtomicBool>,
    events: SyncSender<RuntimeEvent>,
    follower: Option<TranscriptFollower>,
    mut open_transcript: OpenTranscript,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut follower = follower;
        while !stop.load(Ordering::Acquire) {
            // A transcript that does not exist yet is waited for rather than
            // reported: the file appears the moment the agent is first prompted.
            if follower.is_none() {
                match open_transcript() {
                    Ok(opened) => follower = Some(opened),
                    Err(_) => {
                        thread::sleep(TRANSCRIPT_RETRY_DELAY);
                        continue;
                    }
                }
            }
            let Some(follower) = follower.as_mut() else {
                continue;
            };
            let status = if follower_active.load(Ordering::Acquire) {
                AgentStatus::Working
            } else {
                AgentStatus::Done
            };
            match follower.poll_for_status(status) {
                Ok(items) if !items.is_empty() => {
                    let _ = events.send(RuntimeEvent::Transcript(items));
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = events.send(RuntimeEvent::TranscriptError(error.to_string()));
                }
            }
            thread::sleep(Duration::from_millis(100));
        }
    })
}

fn spawn_actions(
    stop: Arc<AtomicBool>,
    events: SyncSender<RuntimeEvent>,
    commands: Receiver<ActionCommand>,
    transport: AgentTransport,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let command = match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(command) => command,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let event = match command {
                ActionCommand::Submit {
                    local_id,
                    text,
                    expected_attachments,
                } => RuntimeEvent::Submitted {
                    local_id,
                    result: transport
                        .submit(&text, expected_attachments)
                        .map_err(|error| error.to_string()),
                },
                ActionCommand::Interrupt => RuntimeEvent::Interrupted(
                    transport.interrupt().map_err(|error| error.to_string()),
                ),
                ActionCommand::Interaction(input) => {
                    let result = match input {
                        InteractionInput::Text(text) => transport.forward_interaction_text(&text),
                        InteractionInput::Key(key) => transport.forward_interaction_key(key),
                    };
                    RuntimeEvent::InteractionForwarded(result.map_err(|error| error.to_string()))
                }
                ActionCommand::LocalImage { attachment } => RuntimeEvent::ImageForwarded {
                    attachment,
                    result: transport
                        .forward_local_image_paste()
                        .map_err(|error| error.to_string()),
                },
                ActionCommand::StagedImage { attachment, path } => RuntimeEvent::ImageForwarded {
                    attachment,
                    result: transport
                        .forward_staged_image(&path)
                        .map_err(|error| error.to_string()),
                },
                ActionCommand::RemoveAttachment { id, marker } => RuntimeEvent::AttachmentRemoved {
                    id,
                    result: transport
                        .remove_attachment(marker)
                        .map_err(|error| error.to_string()),
                },
            };
            let _ = events.send(event);
        }
    })
}

fn spawn_capture_worker(
    stop: Arc<AtomicBool>,
    events: SyncSender<RuntimeEvent>,
    commands: Receiver<CaptureCommand>,
    transport: AgentTransport,
    kind: AgentKind,
) -> JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            let command = match commands.recv_timeout(Duration::from_millis(100)) {
                Ok(command) => command,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            };
            let (event, diagnostic) = resolve_capture_command(
                command,
                kind,
                || transport.recent_unwrapped_ansi(CAPTURE_LINES),
                CAPTURE_ATTEMPTS,
                CAPTURE_RETRY_DELAY,
            );
            let _ = events.send(event);
            if let Some(diagnostic) = diagnostic {
                let _ = events.send(RuntimeEvent::CaptureDiagnostic(diagnostic));
            }
        }
    })
}

fn resolve_capture_command(
    command: CaptureCommand,
    kind: AgentKind,
    read: impl FnMut() -> AppResult<String>,
    attempts: usize,
    retry_delay: Duration,
) -> (RuntimeEvent, Option<String>) {
    let text_fingerprint = fingerprint(&command.canonical_text);
    let (presentation, diagnostic) =
        resolve_capture(kind, &command.canonical_text, read, attempts, retry_delay);
    (
        RuntimeEvent::FinalPresentation {
            stable_id: command.stable_id,
            text_fingerprint,
            presentation,
        },
        diagnostic,
    )
}

/// Chrome that has to share the pane with the answer: the rule above it, the
/// rule below it and the composer line.
const CAPTURE_CHROME_LINES: usize = 3;

fn resolve_capture(
    kind: AgentKind,
    canonical_text: &str,
    mut read: impl FnMut() -> AppResult<String>,
    attempts: usize,
    retry_delay: Duration,
) -> (MessagePresentation, Option<String>) {
    let fallback = style_markdown(canonical_text);
    let expected_lines = fallback.text.split('\n').count();
    let mut last_error = None;
    for attempt in 0..attempts {
        match read() {
            Ok(ansi) => {
                if let Some(styled) = extract_native_final(&ansi, &fallback.text, kind) {
                    return (MessagePresentation::NativeAnsi(styled), None);
                }
                if !answer_can_fit(&ansi, expected_lines) {
                    break;
                }
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        if attempt + 1 < attempts {
            thread::sleep(retry_delay);
        }
    }
    (MessagePresentation::MarkdownFallback, last_error)
}

/// Whether the pane could hold the answer at all.
///
/// The captured window is the visible screen — the multiplexer exposes no
/// source that reaches further — so an answer taller than the pane can never
/// match, and re-reading it eight times only spends a socket round trip per
/// attempt to learn that again.
fn answer_can_fit(ansi: &str, expected_lines: usize) -> bool {
    let pane_lines = sanitize_ansi(ansi).text.split('\n').count();
    expected_lines.saturating_add(CAPTURE_CHROME_LINES) <= pane_lines
}

#[cfg(test)]
mod tests {
    use super::{
        ActionCommand, CaptureCommand, RuntimeEvent, UiRuntime, complete_observation,
        follower_is_active, resolve_capture, resolve_capture_command, spawn_follower,
        spawn_lifecycle_worker,
    };
    use crate::agent::claude::ClaudeAdapter;
    use crate::agent::follower::{FollowerEvent, TranscriptFollower};
    use crate::agent::{AgentIdentity, AgentKind, AgentStatus};
    use crate::composer::NativeComposerState;
    use crate::herdr::HerdrClient;
    use crate::style::{AnsiColor, MessagePresentation};
    use crate::{AppError, AppResult};
    use std::cell::Cell;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    static NEXT_LIFECYCLE_SERVER: AtomicU64 = AtomicU64::new(1);

    fn lifecycle_server(
        response: Result<serde_json::Value, serde_json::Value>,
    ) -> (
        PathBuf,
        PathBuf,
        Receiver<serde_json::Value>,
        SyncSender<()>,
        JoinHandle<()>,
    ) {
        let directory = std::env::temp_dir().join(format!(
            "herdr-simple-prompts-lifecycle-{}-{}",
            std::process::id(),
            NEXT_LIFECYCLE_SERVER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (request_tx, request_rx) = sync_channel(1);
        let (release_tx, release_rx) = sync_channel(1);
        let worker = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&request).unwrap();
            request_tx.send(request.clone()).unwrap();
            release_rx.recv().unwrap();
            let envelope = match response {
                Ok(result) => serde_json::json!({"id": request["id"], "result": result}),
                Err(error) => serde_json::json!({"id": request["id"], "error": error}),
            };
            serde_json::to_writer(&mut stream, &envelope).unwrap();
            stream.write_all(b"\n").unwrap();
        });
        (directory, socket, request_rx, release_tx, worker)
    }

    fn lifecycle_sequence_server(
        responses: Vec<Result<serde_json::Value, serde_json::Value>>,
    ) -> (
        PathBuf,
        PathBuf,
        Receiver<serde_json::Value>,
        JoinHandle<()>,
    ) {
        let directory = std::env::temp_dir().join(format!(
            "hsp-lc-seq-{}-{}",
            std::process::id(),
            NEXT_LIFECYCLE_SERVER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let socket = directory.join("herdr.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (request_tx, request_rx) = sync_channel(responses.len());
        let worker = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&request).unwrap();
                request_tx.send(request.clone()).unwrap();
                let envelope = match response {
                    Ok(result) => serde_json::json!({"id": request["id"], "result": result}),
                    Err(error) => serde_json::json!({"id": request["id"], "error": error}),
                };
                serde_json::to_writer(&mut stream, &envelope).unwrap();
                stream.write_all(b"\n").unwrap();
            }
        });
        (directory, socket, request_rx, worker)
    }

    /// An agent that has not been prompted yet has no transcript on disk. The
    /// overlay waits for the file instead of refusing to open, or a freshly
    /// created agent could not be viewed at all.
    #[test]
    fn a_transcript_that_does_not_exist_yet_is_waited_for() {
        let path = std::env::temp_dir().join(format!(
            "herdr-simple-prompts-late-transcript-{}-{}.jsonl",
            std::process::id(),
            NEXT_LIFECYCLE_SERVER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        let opened = Arc::new(AtomicU64::new(0));
        let attempts = Arc::clone(&opened);
        let transcript = path.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let (events_tx, events_rx) = sync_channel(8);
        let worker = spawn_follower(
            Arc::clone(&stop),
            Arc::new(AtomicBool::new(false)),
            events_tx,
            None,
            Box::new(move || {
                if attempts.fetch_add(1, Ordering::Relaxed) < 2 {
                    return Err(AppError::new("transcript", "not written yet"));
                }
                TranscriptFollower::new(&transcript, Box::new(ClaudeAdapter::default()))
            }),
        );

        std::fs::write(
            &path,
            "{\"type\":\"user\",\"uuid\":\"u1\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"hello\"}]}}\n",
        )
        .unwrap();
        let event = events_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the follower picks the transcript up once it appears");
        assert!(matches!(event, RuntimeEvent::Transcript(_)));
        assert!(opened.load(Ordering::Relaxed) >= 3, "it kept trying");

        stop.store(true, Ordering::Release);
        worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn lifecycle_worker_emits_source_closed_once_and_exits() {
        let (directory, socket, request_rx, release_tx, server) =
            lifecycle_server(Ok(serde_json::json!({
                "type": "wait_matched",
                "event": {"event": "pane_closed", "data": {"pane_id": "w1:p1"}}
            })));
        let client = HerdrClient::connect(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = sync_channel(2);
        let worker = spawn_lifecycle_worker(Arc::clone(&stop), event_tx, client, "w1:p1".into());

        assert_eq!(request_rx.recv().unwrap()["method"], "events.wait");
        release_tx.send(()).unwrap();
        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            RuntimeEvent::SourcePaneClosed
        ));
        worker.join().unwrap();
        assert!(event_rx.try_recv().is_err());

        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lifecycle_worker_timeout_emits_no_event_and_stops_after_poll() {
        let (directory, socket, request_rx, release_tx, server) = lifecycle_server(Err(
            serde_json::json!({"code": "timeout", "message": "timed out waiting for event match"}),
        ));
        let client = HerdrClient::connect(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = sync_channel(1);
        let worker = spawn_lifecycle_worker(Arc::clone(&stop), event_tx, client, "w1:p1".into());

        request_rx.recv().unwrap();
        stop.store(true, Ordering::Release);
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert!(event_rx.try_recv().is_err());

        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lifecycle_worker_detects_close_missed_between_wait_calls() {
        let (directory, socket, request_rx, server) = lifecycle_sequence_server(vec![
            Err(serde_json::json!({
                "code": "timeout",
                "message": "timed out waiting for event match"
            })),
            Err(serde_json::json!({"code": "pane_not_found", "message": "pane missing"})),
        ]);
        let client = HerdrClient::connect(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = sync_channel(1);
        let worker = spawn_lifecycle_worker(Arc::clone(&stop), event_tx, client, "w1:p1".into());

        assert!(matches!(
            event_rx.recv_timeout(Duration::from_secs(1)).unwrap(),
            RuntimeEvent::SourcePaneClosed
        ));
        worker.join().unwrap();
        let methods = [
            request_rx.recv().unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned(),
            request_rx.recv().unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned(),
        ];
        assert_eq!(methods, ["events.wait", "pane.get"]);

        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn lifecycle_worker_keeps_running_when_the_source_pane_still_exists() {
        let (directory, socket, request_rx, server) = lifecycle_sequence_server(vec![
            Err(serde_json::json!({
                "code": "timeout",
                "message": "timed out waiting for event match"
            })),
            Ok(serde_json::json!({
                "type": "pane_info",
                "pane": {"pane_id": "w1:p1"}
            })),
        ]);
        let client = HerdrClient::connect(&socket).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let (event_tx, event_rx) = sync_channel(1);
        let worker = spawn_lifecycle_worker(Arc::clone(&stop), event_tx, client, "w1:p1".into());

        let methods = [
            request_rx.recv().unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned(),
            request_rx.recv().unwrap()["method"]
                .as_str()
                .unwrap()
                .to_owned(),
        ];
        stop.store(true, Ordering::Release);
        worker.join().unwrap();

        assert_eq!(methods, ["events.wait", "pane.get"]);
        assert!(event_rx.try_recv().is_err());
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn runtime_drop_disconnects_a_full_event_queue_before_joining_workers() {
        let (action_tx, _action_rx) = sync_channel(1);
        let (capture_tx, _capture_rx) = sync_channel(1);
        let (event_tx, events) = sync_channel(1);
        event_tx
            .send(RuntimeEvent::CaptureDiagnostic("full".into()))
            .unwrap();
        let blocked_sender = std::thread::spawn(move || {
            let _ = event_tx.send(RuntimeEvent::SourcePaneClosed);
        });
        let runtime = UiRuntime {
            action_tx,
            capture_tx,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            threads: vec![blocked_sender],
        };

        let started = Instant::now();
        drop(runtime);

        assert!(started.elapsed() < Duration::from_millis(250));
    }

    #[test]
    fn lifecycle_retry_backoff_is_bounded() {
        let mut delay = super::LIFECYCLE_RETRY_INITIAL;
        let expected = [200, 400, 800, 1_000, 1_000];
        for expected_ms in expected {
            assert_eq!(delay, Duration::from_millis(expected_ms));
            delay = super::next_lifecycle_retry(delay);
        }
    }

    #[test]
    fn submit_only_enqueues_and_never_waits_for_herdr_io() {
        let (action_tx, action_rx) = sync_channel(1);
        let (capture_tx, _capture_rx) = sync_channel(1);
        let (_event_tx, events) = sync_channel(1);
        let runtime = UiRuntime {
            action_tx,
            capture_tx,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };

        let started = Instant::now();
        runtime.submit("local-1".into(), "hello".into(), 2).unwrap();

        assert!(started.elapsed() < Duration::from_millis(20));
        assert!(matches!(
            action_rx.try_recv().unwrap(),
            ActionCommand::Submit { local_id, text, expected_attachments }
                if local_id == "local-1" && text == "hello" && expected_attachments == 2
        ));
    }

    #[test]
    fn full_action_queue_fails_instead_of_blocking_the_ui() {
        let (action_tx, _action_rx) = sync_channel(1);
        let (capture_tx, _capture_rx) = sync_channel(1);
        let (_event_tx, events) = sync_channel(1);
        let runtime = UiRuntime {
            action_tx,
            capture_tx,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        runtime.submit("local-1".into(), "first".into(), 0).unwrap();

        let started = Instant::now();
        let error = runtime
            .submit("local-2".into(), "second".into(), 0)
            .unwrap_err();

        assert!(started.elapsed() < Duration::from_millis(20));
        assert!(error.to_string().contains("queue is full"));
    }

    #[test]
    fn capture_queue_is_bounded_and_enqueue_never_waits_for_io() {
        let (action_tx, _action_rx) = sync_channel(1);
        let (capture_tx, capture_rx) = sync_channel(1);
        let (_event_tx, events) = sync_channel(1);
        let runtime = UiRuntime {
            action_tx,
            capture_tx,
            events,
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        assert!(
            runtime
                .capture_final("answer-1".into(), "first".into())
                .unwrap()
        );

        let started = Instant::now();
        let queued = runtime
            .capture_final("answer-2".into(), "second".into())
            .unwrap();

        assert!(started.elapsed() < Duration::from_millis(20));
        assert!(
            !queued,
            "a saturated queue drops the request, it does not fail"
        );
        assert_eq!(
            capture_rx.try_recv().unwrap(),
            CaptureCommand {
                stable_id: "answer-1".into(),
                canonical_text: "first".into(),
            }
        );
    }

    #[test]
    fn capture_resolution_returns_native_style_on_first_exact_match() {
        let mut reads = 0;
        let (presentation, diagnostic) = resolve_capture(
            AgentKind::Codex,
            "answer",
            || {
                reads += 1;
                Ok("────────\n\u{1b}[32m• answer\u{1b}[0m\n────────\n› Write a prompt".into())
            },
            8,
            Duration::ZERO,
        );

        assert_eq!(reads, 1);
        assert!(diagnostic.is_none());
        let MessagePresentation::NativeAnsi(styled) = presentation else {
            panic!("exact capture must keep native ANSI provenance")
        };
        assert_eq!(styled.text, "answer");
        assert_eq!(styled.runs[0].foreground, Some(AnsiColor::Green));
    }

    /// The captured window is the visible screen, so an answer taller than the
    /// pane can never match — retrying only spends a round trip per attempt.
    #[test]
    fn capture_resolution_stops_when_the_answer_cannot_fit_the_pane() {
        let canonical = (0..40)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut reads = 0;
        let (presentation, diagnostic) = resolve_capture(
            AgentKind::Codex,
            &canonical,
            || {
                reads += 1;
                Ok("────────\n• something else\n────────\n› Write a prompt".into())
            },
            8,
            Duration::ZERO,
        );

        assert_eq!(reads, 1, "an impossible match must not be retried");
        assert!(diagnostic.is_none());
        assert_eq!(presentation, MessagePresentation::MarkdownFallback);
    }

    #[test]
    fn capture_resolution_keeps_retrying_while_the_answer_still_fits() {
        let mut reads = 0;
        let (presentation, _) = resolve_capture(
            AgentKind::Codex,
            "answer",
            || {
                reads += 1;
                if reads < 3 {
                    Ok("────────\n• still streaming\n────────\n› Write a prompt".into())
                } else {
                    Ok("────────\n\u{1b}[32m• answer\u{1b}[0m\n────────\n› Write a prompt".into())
                }
            },
            8,
            Duration::ZERO,
        );

        assert_eq!(reads, 3, "a possible match must still be retried");
        assert!(matches!(presentation, MessagePresentation::NativeAnsi(_)));
    }

    #[test]
    fn capture_resolution_projects_canonical_markdown_before_exact_match() {
        let canonical = "# Final **answer** with [docs](https://example.test)";
        let (presentation, diagnostic) = resolve_capture(
            AgentKind::Codex,
            canonical,
            || {
                Ok(concat!(
                    "────────\n",
                    "\u{1b}[32m• Final answer with docs\u{1b}[0m\n",
                    "────────\n",
                    "› Write a prompt",
                )
                .into())
            },
            1,
            Duration::ZERO,
        );

        assert!(diagnostic.is_none());
        let MessagePresentation::NativeAnsi(styled) = presentation else {
            panic!("projected exact capture must keep native ANSI provenance")
        };
        assert_eq!(styled.text, "Final answer with docs");
        assert_eq!(styled.runs[0].foreground, Some(AnsiColor::Green));
    }

    #[test]
    fn capture_resolution_projects_local_file_path_before_exact_match() {
        let canonical = "Basis: [TDD](/Users/example/SKILL.md).";
        let (presentation, diagnostic) = resolve_capture(
            AgentKind::Codex,
            canonical,
            || {
                Ok(concat!(
                    "────────\n",
                    "• Basis: \u{1b}[34;4m/Users/example/SKILL.md\u{1b}[0m.\n",
                    "────────\n",
                    "› Write a prompt",
                )
                .into())
            },
            1,
            Duration::ZERO,
        );

        assert!(diagnostic.is_none());
        let MessagePresentation::NativeAnsi(styled) = presentation else {
            panic!("projected local path must keep native ANSI provenance")
        };
        assert_eq!(styled.text, "Basis: /Users/example/SKILL.md.");
        let path_start = styled.text.find("/Users/example/SKILL.md").unwrap();
        let path_style = styled
            .runs
            .iter()
            .find(|run| run.start_byte <= path_start && path_start < run.end_byte)
            .expect("captured path style");
        assert_eq!(path_style.foreground, Some(AnsiColor::Blue));
        assert!(path_style.modifiers.underline);
    }

    #[test]
    fn capture_resolution_is_bounded_and_falls_back_after_read_errors() {
        let mut reads = 0;
        let read: &mut dyn FnMut() -> AppResult<String> = &mut || {
            reads += 1;
            Err(AppError::new("capture", "read unavailable"))
        };

        let (presentation, diagnostic) =
            resolve_capture(AgentKind::Claude, "answer", read, 8, Duration::ZERO);

        assert_eq!(reads, 8);
        assert_eq!(presentation, MessagePresentation::MarkdownFallback);
        assert!(diagnostic.unwrap().contains("read unavailable"));
    }

    #[test]
    fn capture_command_event_carries_stable_id_and_canonical_fingerprint() {
        let command = CaptureCommand {
            stable_id: "answer-1".into(),
            canonical_text: "canonical".into(),
        };

        let (event, diagnostic) = resolve_capture_command(
            command,
            AgentKind::Codex,
            || Ok("no exact candidate".into()),
            1,
            Duration::ZERO,
        );

        assert!(diagnostic.is_none());
        assert!(matches!(
            event,
            RuntimeEvent::FinalPresentation {
                stable_id,
                text_fingerprint,
                presentation: MessagePresentation::MarkdownFallback,
            } if stable_id == "answer-1"
                && text_fingerprint == crate::paste::fingerprint("canonical")
        ));
    }

    #[test]
    fn follower_finalizes_from_atomic_status_even_if_ui_observation_is_dropped() {
        let path = std::env::temp_dir().join(format!(
            "herdr-simple-prompts-runtime-claude-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(
            &path,
            std::fs::read("tests/fixtures/claude/simple.jsonl").unwrap(),
        )
        .unwrap();
        let follower = TranscriptFollower::new(&path, Box::new(ClaudeAdapter::default())).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let working = Arc::new(AtomicBool::new(true));
        let (events_tx, events_rx) = sync_channel(8);
        let worker = spawn_follower(
            Arc::clone(&stop),
            Arc::clone(&working),
            events_tx,
            Some(follower),
            Box::new(|| Err(AppError::new("test", "no second transcript"))),
        );

        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RuntimeEvent::Transcript(events) if events.len() == 1
        ));
        working.store(false, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RuntimeEvent::Transcript(events) if events.len() == 1
        ));

        stop.store(true, std::sync::atomic::Ordering::Release);
        worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    fn identity(status: AgentStatus) -> AgentIdentity {
        AgentIdentity {
            pane_id: "w1:p1".into(),
            kind: AgentKind::Codex,
            session_id: "session-1".into(),
            cwd: PathBuf::from("/repo"),
            status,
        }
    }

    #[test]
    fn observation_reads_one_ansi_surface_for_status_composer_and_blocked_view() {
        for status in [
            AgentStatus::Working,
            AgentStatus::Done,
            AgentStatus::Blocked,
        ] {
            let ansi_reads = Cell::new(0);
            let observation = complete_observation(identity(status), || {
                ansi_reads.set(ansi_reads.get() + 1);
                Ok(concat!(
                    "\u{1b}]0;secret\u{7}\u{1b}[32m• question\u{1b}[0m\n",
                    "────────\n",
                    "› \u{1b}[2mWrite a prompt\u{1b}[0m\n",
                    "gpt-5.6-sol xhigh · /repo · weekly 75% left",
                )
                .into())
            })
            .unwrap();

            assert_eq!(ansi_reads.get(), 1);
            assert_eq!(
                observation.status_text,
                "gpt-5.6-sol xhigh · /repo · weekly 75% left"
            );
            assert!(!observation.status_text.contains("question"));
            assert_eq!(observation.native_composer, NativeComposerState::Clear);
            if status == AgentStatus::Blocked {
                let styled = observation.blocked_surface.unwrap().unwrap();
                assert!(styled.text.contains("question"));
                assert!(!styled.runs.is_empty());
            } else {
                assert!(observation.blocked_surface.is_none());
            }
        }
    }

    #[test]
    fn observation_uses_the_primary_row_of_a_wrapped_codex_footer() {
        let observation = complete_observation(identity(AgentStatus::Done), || {
            Ok(concat!(
                "• answer\n",
                "────────\n",
                "› \u{1b}[2mAsk Codex to do anything\u{1b}[0m\n",
                "gpt-5.6-sol high · agent-dashboard · Context 21% used · weekly 62%\n",
                "  left · …",
            )
            .into())
        })
        .unwrap();

        assert_eq!(
            observation.status_text,
            "gpt-5.6-sol high · agent-dashboard · Context 21% used · weekly 62%"
        );
        assert_eq!(observation.native_composer, NativeComposerState::Clear);
    }

    #[test]
    fn runtime_initializes_blocked_status_as_active_for_the_follower() {
        assert!(follower_is_active(AgentStatus::Working));
        assert!(follower_is_active(AgentStatus::Blocked));
        assert!(!follower_is_active(AgentStatus::Done));

        let path = std::env::temp_dir().join(format!(
            "herdr-simple-prompts-runtime-blocked-{}.jsonl",
            std::process::id()
        ));
        std::fs::write(
            &path,
            std::fs::read("tests/fixtures/claude/simple.jsonl").unwrap(),
        )
        .unwrap();
        let follower = TranscriptFollower::new(&path, Box::new(ClaudeAdapter::default())).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicBool::new(follower_is_active(AgentStatus::Blocked)));
        let (events_tx, events_rx) = sync_channel(8);
        let worker = spawn_follower(
            Arc::clone(&stop),
            Arc::clone(&active),
            events_tx,
            Some(follower),
            Box::new(|| Err(AppError::new("test", "no second transcript"))),
        );

        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RuntimeEvent::Transcript(events) if events.len() == 1
                && matches!(
                    events.as_slice(),
                    [FollowerEvent::Conversation(crate::model::ConversationEvent::User(_))]
                )
        ));
        assert!(events_rx.recv_timeout(Duration::from_millis(250)).is_err());
        active.store(false, std::sync::atomic::Ordering::Release);
        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            RuntimeEvent::Transcript(events) if matches!(
                events.as_slice(),
                [FollowerEvent::Conversation(crate::model::ConversationEvent::Final(_))]
            )
        ));

        stop.store(true, std::sync::atomic::Ordering::Release);
        worker.join().unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
