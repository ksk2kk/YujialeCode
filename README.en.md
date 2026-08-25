# YujialeCode

A code agent built for local model enthusiasts. Pure Rust, an extremely minimal system prompt, and smooth operation even at a 30-token speed budget. Fully compatible with Claude-style skills.

A minimal pure-Rust local model CLI agent: few dependencies (8 crates: serde / ureq / reqwest / tokio / tungstenite, etc.), a hand-rolled TUI, and a low-token system prompt design. Works with any OpenAI-compatible endpoint including DeepSeek / Ollama / LM Studio / vLLM. The `--mock` offline demo runs the entire flow without any API key. Optional QQ integration (OneBot v11, NapCat / Lagrange) supports fast group chat and private chat responses.

## Dedication

This project is named after my friend [Yujiale](https://github.com/dawalishi821) — the "Yujiale" in YujialeCode is his name. Happy birthday, Yujiale!

## Features

- Short system prompt: native mode does not repeat function schemas, only states the role, tool entry points, and stop conditions; text mode only adds the tool format and one Read example. Detailed parameters are returned on demand by `list_tools`.
- Dual tool-call protocols: text mode parses ```` ```tool {...} ```` code blocks; native mode registers `readline`, `execute_command`, `list_tools`, and `ask_user` as core entries, with the rest of the tools discovered dynamically.
- Cross-platform Computer Use: native screenshot, window enumeration, mouse, keyboard, and scrolling backends for Linux Wayland, macOS, and Windows. Retina/DPI scaling, multi-display offsets, and stale frames are handled automatically; action batches execute in order and return only one fresh full screenshot to the local vision model.
- Proactive goal follow-up: `/FuckMaster` schedules progress check-ins. Reminders queue while the agent is busy and are delivered through the TUI/QQ after it becomes idle; QQ output is normalized to plain text without Markdown or emoji in the transport layer.
- Weak-model compatible routing: auto-corrects wrong tool names, parameter names, nesting levels, stringified JSON, and stringified numbers/booleans/arrays; forcibly redirects when the model picks the wrong tool but the intent is clear.
- Aggregated web research: `web_search` concurrently aggregates DuckDuckGo, Bing, and configured Brave/SearXNG, normalizing URLs, deduplicating across sources, ranking by quality, and filtering domains; `web_research` runs complementary queries and fetches curated page bodies in a single call.
- Fault-tolerant editing: `editline` matches exactly first, then tolerates CRLF, trailing spaces, smart quotes, Unicode dashes, and special whitespace; only unambiguous matches are applied.
- Enforced Read channel: compatible with `Read/read_file/readline` and `file_path/offset/limit`; Read is the first entry in the native tool list, reads the first 2000 lines by default, returns complete pages, and gives the exact next-page offset. A simple `cat FILE` is hard-converted to Read before execution; complex cat calls are rejected outright.
- Non-collapsing directory listing: `ls/dir/list_directory/listdir` all route into a continuous pager (200 entries by default, up to 1000), with an exact `Next page` call at the end of each page; simple `ls -la` commands are also hard-converted.
- Structured questions: `ask_user` follows the Claude Code `question/header/options/multiSelect` protocol with 1-4 questions, single/multi select, and automatic Other.
- Hard-serialized foreground turns: messages sent while generating join a FIFO queue; Ctrl+C, process errors, and repeated `ask_user` never release the turn lock early.
- Control-layer anti-spin: circuit breaker on the third identical call, three identical results in a row, or four consecutive failures; when the tool limit is hit or the model is corrupted, a final tool-free wrap-up is executed instead of returning nothing.
- Large results externalized: oversized tool results are saved in full to `~/.yjlcoder/tool-results/`.
- Automatic context compression: ported from openai/codex (see the attribution below), triggered automatically when usage exceeds the threshold, or manually via `/compress`.
- Multiple sessions: one jsonl per session, `/new /ls /use /rm`.
- QQ bridge: OneBot v11 reverse-WS server (NapCat reverse connection) or forward client; allowlist plus trigger-word / @ filtering, one session per chat, debounce merging, fast responses.
- Skill installation: `/install pdf` pulls SKILL.md from anthropics/claude-code, or installs from any URL / local directory.

## Build and Run

```bash
cargo build --release

# Run after configuring your API key
yjlcoder

# Offline demo (no key needed; TUI/tool loop/compression all work)
yjlcoder --mock

# QQ bridge (background) + TUI
yjlcoder --qq

# QQ bridge daemon only (no TUI)
yjlcoder --qq-only

# Temporarily switch models
yjlcoder --model deepseek-v4-flash
```

On first run, `~/.yjlcoder/config.json` is generated automatically. The `YJLCODER_HOME` environment variable overrides the config directory.

## Configuration

```json
{
  "provider": {
    "base_url": "https://api.deepseek.com",
    "api_key": "",
    "model": "deepseek-v4-flash",
    "ctx_window": 1000000,
    "native_tools": true,
    "timeout_secs": 120
  },
  "qq": {
    "ws_mode": "server",
    "ws_addr": "127.0.0.1:6701",
    "ws_path": "/onebot/v11/ws",
    "groups": [],
    "users": [],
    "triggers": ["yjlcoder"],
    "need_at": true,
    "max_tokens": 1024
  },
  "tui": {
    "compress_threshold": 0.75,
    "tool_result_max_tokens": 1000
  },
  "fuckloop": true
}
```

- `provider.native_tools: true` uses native function calling (recommended for DeepSeek); `false` uses the text protocol (recommended for small local models).
- `provider.ctx_window` is the fallback context window for cloud models: the DeepSeek v4 family has a 1M-token window (`deepseek-v4-flash` / `deepseek-v4-pro`) — set the real window and the compression threshold is computed from it; do not set a small value for cloud models. Local llama.cpp servers auto-detect the window (taking priority over config); Ollama / LM Studio cannot be detected, so fill in the actual model window.
- Empty `qq.groups` / `qq.users` denies everyone by default; messages matching `triggers` or mentioning the bot trigger a reply.
- Permission separation: `qq.admins` (admin QQ) can let the agent operate the computer (all tools); non-admins can only chat.
- Quick commands: `/qqadmin <QQ>` adds an admin, `/qqgroup <group>` adds a group; effective after the QQ bridge restarts.
- Local model example: `base_url: http://localhost:11434/v1, model: qwen2.5-coder:7b, native_tools: false` (Ollama); LM Studio is `http://localhost:1234/v1`.

## QQ Bridge (OneBot v11)

### Docker Deployment with NapCat (recommended; one-click scripts in `deploy/napcat/`)

```bash
mkdir -p ~/.config/yjlcoder
install -m 600 deploy/deploy.env.example ~/.config/yjlcoder/deploy.env
# Edit deploy.env and set your own token; the QQ account may stay empty for QR login
deploy/napcat/start.sh --no-follow
```

1. `deploy.env` is the only machine-private configuration. Binary path, QQ account, WebUI token, container name, data directories, ports, and OneBot URL are all configurable; every script also accepts `--config FILE`.
2. Open the configured NapCat WebUI address and scan the QR code. Real tokens never belong in repository files or scripts.
3. NapCat connects to `YJLCODER_ONEBOT_WS_URL` after login; use `deploy/napcat/config/onebot11.example.json` for a manual setup.
4. `deploy/install-user-services.sh --enable` installs systemd user services without embedding a username or repository path.
5. Default trigger: send `yjlcoder hi` in a group, or `@bot hi` (with `need_at`).

Manual setup (existing NapCat/Lagrange): create a reverse WebSocket connection in NapCat and point it at `ws://127.0.0.1:6701/onebot/v11/ws`.

Behavior details:

- One independent session per group / private chat (`qq_g<group>` / `qq_u<QQ>`), long sessions are reused.
- New messages arriving while a chat is generating are queued with only the latest kept (debounce merging), processed serially.
- Chat mode caps `max_tokens` for fast responses; the agent can proactively message any group/friend via `qq_send`.
- Untriggered messages are recorded but not forwarded.

## Context Compression (ported from openai/codex, Apache-2.0)

The compression module is a line-by-line port of [openai/codex](https://github.com/openai/codex) (Apache-2.0), with per-item attribution:

| This repo | codex source |
| --- | --- |
| `approx_token_count` / `approx_bytes_for_tokens` / middle truncation | `codex-rs/utils/string/src/truncate.rs` (4 bytes approx 1 token) |
| `SUMMARIZATION_PROMPT` | verbatim `codex-rs/prompts/templates/compact/prompt.md` |
| `SUMMARY_PREFIX` | verbatim `codex-rs/prompts/templates/compact/summary_prefix.md` |
| `build_compacted_history` | `codex-rs/core/src/compact.rs:639-717` |
| drop-old-retry on context overflow | `codex-rs/core/src/compact.rs:309-324` |
| `formatted_truncate_text` | `codex-rs/utils/output-truncation/src/lib.rs` |

Algorithm: full history plus a compression prompt sent to the model, the last assistant output is taken as the summary, the summary is prepended with `SUMMARY_PREFIX` as the final user message, and assistant / tool messages are dropped. Manual compression keeps at most the most recent 20k tokens of user messages; automatic compression computes a budget dynamically from the current model window, compacting to roughly 80% of the trigger line to avoid re-compressing every turn on small windows.

## Sessions and Skills

- Session files live in `~/.yjlcoder/sessions/<id>.jsonl`; `/new [id]`, `/ls`, `/use <id>`, `/rm <id>` (cannot delete the current one).
- `/skills` lists installed skills; `/install <name>` installs from the anthropics/claude-code repo (e.g. `pdf`), and also supports `/install <URL>` or a local directory; `run_skill <name>` injects the SKILL.md into the context.

## Development

```bash
cargo build                 # zero warnings
cargo test --all-targets    # unit tests + --mock end-to-end (tool loop, circuit breaker, auto compression, TUI frame assertions)
cargo clippy                # zero warnings
```

TUI frame rendering has snapshot-style assertions (`frame_visual_elements`); visual changes must pass it first.

## Architecture

```
src/
  main.rs        entry: --mock / --qq / --qq-only / --model, TUI main loop
  config.rs      ~/.yjlcoder/config.json
  prompt.rs      low-token system prompt
  tool_compat.rs weak-model tool name/parameter coercion routing
  llm.rs         OpenAI-compatible streaming client (SSE) + Mock offline model
  registry.rs    tool category registry (list_tools rendering)
  tools.rs       all op implementations (shell/file/net/sec/session/ctx/qq) + fault-tolerant editing
  web.rs         multi-backend aggregate search, deep research, batch page fetch
  agent.rs       main loop: parse tool block → execute → feed back → loop (max_iter=8)
  session.rs     multi-session jsonl storage
  compress.rs    codex-ported context compression
  tui.rs         hand-rolled TUI (ANSI + termios)
  qq.rs          OneBot v11 bridge (reverse-WS server / forward client)
  skills.rs      skill install / list / inject
```

Threading model: main rendering loop + stdin input thread + one agent worker thread per turn (`std::sync::mpsc`), with the QQ bridge on its own thread. The rest uses blocking I/O on std threads; only llm.rs runs a single-threaded tokio runtime for cancellable reqwest streaming SSE requests. Direct dependencies (8): serde / serde_json / ureq / reqwest / tokio / tungstenite / libc / unicode-width.

## Design References

Engineering design draws on [claude-code-best/claude-code](https://github.com/claude-code-best/claude-code) for tool-pool stabilization, oversized tool-result persistence, session recovery, permission tiers, and task-state observability; on [Exa MCP Server](https://github.com/exa-labs/exa-mcp-server) for search/fetch layering, query diversification, hard filtering, deduplication, and source-quality strategies; and on [Pi Agent Harness](https://github.com/earendil-works/pi) for parameter preprocessing, execution state machines, truncation guards, fault-tolerant editing, and large-output persistence. YujialeCode keeps an independent Rust implementation and short system prompt and copies no reference project's system prompt. Pinned reference versions are listed in `THIRD_PARTY_NOTICES.md`.

## License

This project is licensed under [GPL-3.0-only](LICENSE).

Note: the context compression module is ported from [openai/codex](https://github.com/openai/codex) (Apache-2.0). Apache-2.0 is compatible with GPLv3; the corresponding attribution is in the table above.
