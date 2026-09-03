# Changelog

## Unreleased

- Render user-message bands with Herdr One Light's dark text and light
  `surface1` background instead of a dark-theme-only white-on-charcoal pair.
- Support Codex CLI 0.153 responsive status lines at narrow/mobile widths,
  including wrapped usage fields and layouts that omit the working directory.
  Composer detection, native answer styling, and displayed status use the same
  conservative footer rules.
- Recognize the exact active-turn `tab to queue message` composer so an unsent
  steer can be adopted into an empty Simple Prompts draft, edited, and sent.
  Malformed lookalikes still fail closed, and failed adoption leaves the native
  draft intact with a clearer return-to-Codex message.

## 0.3.0

- Background task notifications no longer read as prompts you typed. Claude Code
  queues a finished background command and then writes the same text back as an
  ordinary user record when it dequeues; only the envelope says the system wrote
  it, so the body was rendered as a prompt like any other. Both shapes are now
  dropped. A message relayed in from a coordinator session is someone addressing
  the session and still appears.
- Typing works again in a pane whose Claude Code runs a custom `statusLine`.
  Such a pane prints two lines of chrome below the composer - the status line
  and the mode hint - where the reader allowed one, so it declared the composer
  unverifiable and refused every keystroke with `Unable to verify native
  composer`. The boundary now asks who wrote the line below the rule instead of
  counting lines, which also restores native answer colors in those panes: the
  capture path shared the same limit, and the shipping build's bullet had moved
  besides, so its check for an older answer had quietly stopped matching.

## 0.2.0

- History navigation that no longer depends on the composer. `PageUp` and
  `PageDown` move by the view less two rows instead of a fixed five,
  `Shift+Up` and `Shift+Down` move a row whatever the composer holds, and
  `Shift+Home`/`Shift+End` - or `Shift+Alt+Up`/`Shift+Alt+Down` on a keyboard
  without those keys - reach the oldest turn and the newest one. The view
  answers these keys itself and never forwards them, so the conversation stays
  readable while a native question is blocking or the composer is guarded.
- A scroll thumb in the right gutter column whenever the history is taller than
  the pane. It keeps the background of a prompt band underneath, and prompt text
  now wraps one column short of its band, so nothing of the conversation is
  hidden by it. A history that fits shows nothing.
- While the view sits away from the newest answer, the footer leads with the
  number of rows below it and the key that returns.

## 0.1.0

- First release: user prompts and final Codex or Claude answers in a full pane,
  with the native transcript untouched and reasoning, tool traffic, system
  context, and subagent noise left in the native pane.
