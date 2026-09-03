# Herdr Simple Prompts

[![CI](https://github.com/AlexSamarsky/herdr-simple-prompts/actions/workflows/ci.yml/badge.svg)](https://github.com/AlexSamarsky/herdr-simple-prompts/actions/workflows/ci.yml)
[![MIT License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust 1.88+](https://img.shields.io/badge/rust-1.88%2B-orange.svg)
![Herdr 0.7.5+](https://img.shields.io/badge/herdr-0.7.5%2B-6f42c1.svg)
![macOS and Linux](https://img.shields.io/badge/platform-macOS%20%7C%20Linux-lightgrey.svg)

A full-pane view for [Herdr](https://herdr.dev) that shows your prompts and the
agent's final answers - and nothing else. Reasoning, tool traffic, system
context, and subagent noise stay in the native pane, which Simple Prompts never
touches. `prefix+m` switches between the two.

![Simple Prompts showing an English Codex conversation](assets/simple-prompts.png)

These agent versions have been tested on macOS and Linux:

| Agent | Tested versions |
|---|---|
| Codex CLI | `0.146.0`–`0.149.0`, `0.153.0` |
| Claude Code | `2.1.237` |

Newer agent releases may work, but have not yet been verified.

## Install

You need Herdr 0.7.5+, Rust 1.88+ with Cargo, `jq`, and either Codex CLI
`0.146.0`–`0.149.0`/`0.153.0` or Claude Code `2.1.237`. The JSON tool supports
automatic recovery of already-running Codex sessions.

```bash
herdr plugin install JoernStoehler/herdr-simple-prompts
herdr integration install codex   # or: herdr integration install claude
```

Bind the toggle in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+m"
type = "plugin_action"
command = "herdr.simple-prompts.toggle"
description = "Toggle Simple Prompts"
```

```bash
herdr server reload-config
```

Focus a Codex or Claude pane and press the Herdr prefix (normally `ctrl+b`)
followed by `m`. Press it again to return to the unchanged native pane.
If the focused Codex pane is missing native session metadata, `prefix+m`
recovers and verifies only that pane before opening Simple Prompts. Recovery is
fail-closed and never reads transcript contents.

No binary is published: Herdr clones this source and builds it locally with
`cargo build --locked --release`. The plugin has no network access of its own.

## Using it

| Key | Action |
|---|---|
| `Enter` | Submit the prompt |
| `Shift+Enter` / `Ctrl+J` | Newline |
| `Ctrl+V` | Attach an image through the native agent |
| `Esc` | Interrupt the agent while it is working |
| `PageUp` / `PageDown` | Scroll the history by a page |
| `Shift+↑` / `Shift+↓` | Scroll a row, draft in the composer or not |
| `Shift+Home` / `Shift+End` | Jump to the oldest turn, or back to the latest |
| `Shift+Alt+↑` / `Shift+Alt+↓` | The same two ends, without a `Home` key |

- Prompts you type while the agent is working are queued by the agent and show
  up here where they were queued.
- A history taller than the pane shows a thumb in the right-hand column, over
  prompt bands as well as answers; prompt text wraps one column earlier so it
  never runs under it. While the view sits away from the newest answer the
  footer says how far back it is and which key returns. The wheel scrolls too
  where the terminal turns it into arrow keys, but only while the composer is
  empty - the shift keys above always work.
- On a MacBook `Home` and `End` are `fn+←` and `fn+→`, so the jumps are
  `fn+Shift+←` and `fn+Shift+→`, or `Shift+Alt+↑` and `Shift+Alt+↓`.
- A paste of 1,000+ characters collapses into one `[Pasted Content · N chars]`
  token; the agent still receives the complete text.
- Images appear as `[Image #N]` placeholders - the view never renders pixels.
- When the agent asks something, the view shows `INTERACTION REQUIRED` and
  forwards your keys to the native question until it is answered.
- Recognizable unsent text in the native composer is adopted into an empty
  Simple Prompts draft and cleared from the native copy, including a steer
  drafted while Codex is working. If adoption cannot be verified, editing stays
  blocked and `prefix+m` returns to the preserved native draft.
- Dragging selects and copies through Herdr; links are clickable in OSC 8
  terminals.

## Privacy

The native transcript is never modified. Simple Prompts keeps its own private
copy of the prompts and final answers it displayed, scoped to one source pane
and one native session, so reopening the view restores what you saw. Reasoning,
tool traffic, native attachment paths, and submitted large-paste bodies never
enter it. An unsent draft is stored privately so it can survive toggling and
send failures. State files are `0600` inside `0700` directories and are deleted
when the source pane closes.

## Development

```bash
cargo test --all-targets --all-features
cargo build --locked --release
herdr plugin link .
```

## Limitations

- Codex and Claude only; no Windows.
- The view follows the focused session; it is not a conversation browser.
- Clickable labels need OSC 8 support; other terminals show the same styled
  text without the link.
- Agent footer parsing is conservative - unproven status fields are omitted
  rather than guessed.

## Docs

- [Behavior reference](docs/behavior.md) - what is shown, hidden, forwarded, and
  stored, in detail.
- [Development](docs/development.md) - verification gates, manual smoke test,
  publishing.
- [Troubleshooting](docs/troubleshooting.md) - unavailable sessions, the hotkey,
  images, uninstall.
- [Changelog](docs/changelog.md) - what changed in each version.
- [AGENTS.md](AGENTS.md) - working agreements for coding agents in this repo.

## License

MIT
