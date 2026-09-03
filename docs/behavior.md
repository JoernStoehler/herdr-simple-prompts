# Behavior reference

Everything Simple Prompts shows, hides, forwards, and stores. The README is the
short version; this file is the contract.

## Pane targeting

Simple Prompts opens as a targeted zoomed plugin pane next to the exact source
pane and immediately fills that source tab. It uses Herdr's explicit
`target_pane_id` path, so a concurrent focus change cannot move the view to a
different pane. Closing the view removes its temporary split and restores the
original layout.

The toggle action scopes session recovery to Herdr's `HERDR_PANE_ID` action
context before it starts the Rust toggle. An already registered pane, or the
Simple Prompts overlay pane used while closing the view, needs no recovery and
continues directly to the toggle. A missing action pane is an error.
The recovery helper invokes the host CLI through Herdr's authoritative
`HERDR_BIN_PATH`; plugin actions do not depend on `herdr` being discoverable in
their process `PATH`.

For an unregistered Codex pane, recovery reads only the final visible native
footer and requires exactly one footer session id plus exactly one matching
local transcript filename. It never reads transcript contents. After reporting
the candidate, it reads the Herdr agent record back and requires the same
id-based `herdr:codex` metadata. Any ambiguity, read failure, rejected report,
or unretained metadata stops the action before the Rust toggle can open a view.
No other pane's surface or transcript filenames are inspected, and no other
pane is registered by that action.

The `zoomed` placement is intentional: Herdr 0.7.5 overlays target the active
pane and reject `target_pane_id`, which would reintroduce a cross-pane focus
race. See the [plugin pane documentation](https://herdr.dev/docs/plugins/#panes).

`prefix+p` is deliberately not used - Herdr assigns it to the previous tab.

## Conversation view

History is laid out once by a Unicode-aware visual-row engine. The same rows
drive rendering, bottom alignment, scrolling, and sticky prompt context, so a
long answer stays reachable instead of being wrapped a second time by the
terminal widget.

Codex conversation records are accepted in both the legacy `event_msg` layout
and the current `response_item/message` layout. In the current layout only user
`input_text` and assistant `output_text` with `phase = "final_answer"` become
visible messages. Developer context, assistant commentary, reasoning, tool
traffic, subagent records, and unknown content items stay hidden. Opening the
view reads the supported visible history from the beginning, then follows new
records appended to the same native transcript.

- Each user prompt is a full-width neutral-gray block. Its top gray row carries
  the local `DD.MM.YYYY HH:MM` timestamp in undimmed gray, and one blank gray
  row stays below the text. There is no `YOU` label. Records without a valid
  timestamp keep the top row blank.
- Each final answer starts with its local `DD.MM.YYYY HH:MM` timestamp on one
  undimmed gray, unboxed row, followed immediately by the styled answer text.
  There is no `ANSWER` label or answer box. Records without a valid timestamp
  add no metadata row and no gap.
- A prompt typed while Claude Code is working is queued rather than sent, and
  Claude Code stores it as a queued command instead of a user record. It appears
  in its queued position with its images, like any other prompt. Prompts nobody
  typed stay hidden: a finished background command is queued as a task
  notification and then dequeued as a user record whose body reads like a typed
  prompt, so both shapes are dropped on `promptSource: "system"` and on an
  `origin.kind` of `task-notification`. A message relayed into the session from
  a coordinator is someone addressing the session and stays visible.
- Once a prompt scrolls out of its natural position, at most its first two
  wrapped rows stay at the top, with the gray top padding when the viewport has
  room. The next prompt pushes the old block away one row at a time. The sticky
  copy never replaces or truncates the full prompt in ordinary history.
- `PageUp` and `PageDown` scroll by the view less two rows, so the rows at the
  edge stay on screen. `Shift+Up` and `Shift+Down` move one row, `Shift+Home`
  and `Shift+End` reach the oldest turn and the newest one. These keys answer in
  the view and are never forwarded, so the history stays readable while a native
  question is blocking or the composer is guarded; the bare arrows still scroll
  only while the composer has no text for a cursor to move through. Returning to
  the bottom resumes live bottom-following.
- A history taller than the viewport draws a scroll thumb in the right gutter
  column. Only the glyph and its color are set, so a prompt band keeps its
  background and still reaches both terminal edges, and prompt text wraps one
  column short of the band so no prompt ever runs under the thumb. A history
  that fits leaves both gutters untouched. While the view is away from the
  bottom the footer leads with the number of rows below it and the key that
  returns.
- Mouse capture is deliberately off, so dragging across text uses Herdr's native
  selection and automatic copy, and OSC 8 links stay clickable.

## Answer text and native styles

For every final answer the transcript's `Message.text` stays the canonical
Markdown value used for identity and replay. Simple Prompts projects that
Markdown into visible terminal text: heading, emphasis, inline-code, and
fenced-code delimiters are removed. A Markdown `http://` or `https://` link
becomes a cyan underlined label without the visible destination, clickable in
OSC 8-capable terminals. Other valid schemes stay ordinary labels; malformed
syntax stays literal.

For a newly observed answer the plugin reads recent ANSI output from the source
agent and accepts a styled block only when its sanitized visible text matches
that projection exactly, at one unique known Codex or Claude final-answer
boundary. The captured presentation owns the visible text and safe SGR colors,
bold, dim, italic, and underline. Cursor movement, alternate-screen commands,
OSC titles, hyperlinks, clipboard commands, and other terminal controls are
discarded and never replayed. A clickable link is rebuilt only from the
canonical, control-free HTTP(S) destination after the text matched exactly; a
captured OSC sequence can never become link metadata. For a valid but
non-clickable destination only a captured underline on that label is removed.

When exact native ANSI is unavailable, the same projected text is shown with
deterministic fallback styles. Visible text and styles are saved together in the
journal; version-1 records stay readable and are downgraded to fallback when
their legacy style offsets cannot describe the new projection safely.

## Composer

| Key | Action |
|---|---|
| `Enter` | Submit the prompt |
| `Shift+Enter` | Insert a newline when the terminal supports it |
| `Ctrl+J` | Portable newline fallback |
| `Ctrl+V` | Attach an image through the native agent |
| `Esc` | Interrupt the agent while it is working |
| `PageUp` / `PageDown` | Scroll the conversation history by a page |
| `Shift+Up` / `Shift+Down` | Scroll one row regardless of the composer |
| `Shift+Home` / `Shift+End` | Jump to the oldest turn or the newest |
| `Shift+Alt+Up` / `Shift+Alt+Down` | The same two ends for keyboards without `Home` |

Pastes below 1,000 characters stay directly editable. A paste of 1,000
characters or more appears as one atomic `[Pasted Content · N chars]` token in
the composer and in prompt history, while the agent receives the complete
original text with all newlines. Multiple large pastes stay separate; the cursor
skips each token and deletion removes it whole. The plugin truncates no prompt.
Any Herdr or agent-side rejection is shown, and the exact draft - including the
hidden source behind compact tokens - is restored.

## Native composer protection

The plugin editor is separate from the native Codex or Claude composer. Before
editing, and again immediately before submission, the recognized native composer
surface is checked:

- recognizable native text is adopted into an empty Simple Prompts draft when
  the view opens and cleared from the native composer; if it cannot be adopted,
  editing and submission stay disabled and `prefix+m` returns to the preserved
  native draft;
- native image placeholders are accepted only when their count exactly matches
  the images attached by Simple Prompts;
- an unrecognized or incomplete composer layout is blocked conservatively rather
  than assumed empty.

The Claude composer is found between its two rules, and what sits below the
closing rule is chrome, however many lines it runs to - a custom `statusLine`
prints its own above the mode hint. The boundary is rejected only when a line
below it is one Claude authored, which the shipping build opens with a filled
circle and older builds with a filled square. The capture path that lifts native
answer styling reads the same rule, so neither can accept a pane the other
refuses.

The check returns only a coarse state and an attachment count. Native draft text
and native attachment paths never enter plugin state, the journal, logs, or
error messages.

Herdr 0.7.5 has no atomic "inspect and submit" operation. The final preflight
prevents the reported single-client draft concatenation, but cannot guarantee
atomicity if another client writes to the native composer between that read and
`agent.prompt`.

## Questions and approvals

When Herdr reports the source agent as blocked, the view shows
`INTERACTION REQUIRED` and a refreshed, sanitized copy of the native question,
choice, permission, or approval surface. History and composer are hidden during
that mode and return unchanged when the agent unblocks.

Typed and pasted text are forwarded. Supported keys: `Up`, `Down`, `Left`,
`Right`, `Tab`, `Shift+Tab`, `Space`, `Enter`, `Backspace`, `Delete`, `Esc`.
Each accepted input is sent once; unsupported control keys are ignored. Mouse
interaction is not mapped in version 0.1.

If the interaction cannot be read, an error is shown rather than a guess. Press
`prefix+m` and answer in the native pane.

## Images

Locally, `Ctrl+V` is forwarded to the native composer, and the attachment is
recorded only after the native pane exposes its image marker.

During remote attach, Herdr stages the pasted image in its private remote
temporary directory and pastes the path. Simple Prompts accepts only an existing
image file under a `herdr-clipboard-images-*` path and forwards it. Images show
as compact `[Image #N]` placeholders; pixels are never rendered or copied.

## Privacy and state

Simple Prompts never modifies the native transcript. It does keep a private copy
of the visible prompt/final-answer subset so reopening the view reproduces what
it showed. That copy is scoped to one source pane and one native session - not a
global conversation database.

The Herdr-managed state directory holds:

- the source-to-plugin-pane registry;
- the current draft and local attachment placeholders;
- compact-paste display ranges, character counts, and integrity fingerprints;
- the pane/session visible-history journal.

The journal is auditable JSON Lines at:

```text
history/<safe-source-pane-id>/<native-session-id>.jsonl
```

Each versioned record holds only a display-safe prompt or visible final answer,
its native stable and turn identifiers, sanitized attachment labels and IDs, a
timestamp and display order, a text fingerprint, and either validated native
style ranges or fallback/plain presentation provenance. A repeated stable id is
an append-only upsert whose latest valid record wins, so later exact native ANSI
can replace a fallback presentation. Timestamps predate the date/time rows, so
existing history needs no migration.

The journal never stores reasoning, commentary, tool calls or results, system
context, subagent traffic, blocked interaction surfaces, native attachment
paths, or the hidden body of a large paste - a submitted large paste is kept
only as its compact marker. Only an unsent draft may retain the complete hidden
paste, so editing and send-failure recovery stay lossless.

State directories use mode `0700`; registry, draft, namespace, and journal files
use `0600`. Journal writes are asynchronous, append newline-terminated records,
and ignore an incomplete final line during recovery.

Retention follows the source pane, not the view:

| Event | Result |
|---|---|
| Close only Simple Prompts with `prefix+m` | Keep history and draft for reopening |
| Close the native source pane | Delete that pane's registry, draft, compact metadata, and history namespace |
| Reuse a pane for a different native session | Delete the replaced session's state during validation |
| Herdr temporarily reports `agent_not_found` while the source pane still exists | Keep the namespace and overlay mapping; an open overlay remains closable with `prefix+m` |
| Source cannot be verified temporarily | Keep its state; remove it on the next invocation after seven continuously unverifiable days |

Pane existence is authoritative for destructive cleanup. An unavailable agent
record is not treated as proof that its pane was closed; cleanup after that
error requires a confirming pane lookup.

Before every prompt, interrupt, image mutation, or blocked-input forwarding, the
plugin verifies that the source pane still holds the original agent kind and
native session id. No detached cleanup watcher or resident daemon is created.

## Trust model

No executable plugin binary is published or downloaded. Herdr clones this public
source and runs one visible build command locally:

```bash
cargo build --locked --release
```

`Cargo.lock` fixes the whole dependency graph. The plugin has no HTTP client,
telemetry, analytics, update checker, or runtime network access; Cargo itself
still needs crates.io on the first build unless the crate sources are cached.
Herdr shows the manifest and build command in its trust preview before building.

Confirm the built plugin is registered:

```bash
herdr plugin list --plugin herdr.simple-prompts
```
