# Changelog

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
