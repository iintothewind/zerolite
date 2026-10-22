<p align="center">
  <img src="web/public/logo.png" alt="ZeroLite" width="360" />
</p>

<h1 align="center">ZeroLite 1.2.2 — Personal AI Assistant</h1>

<p align="center">
  <strong>A ZeroClaw fork. You own the agent, the data, and the machine it runs on.</strong>
</p>

---

ZeroLite is a fork of [ZeroClaw](https://github.com/zeroclaw-labs/zeroclaw). The binary, config file, and commands stay `zeroclaw`. This tree is crate version `1.2.2`; the release tag is `v1.2.2`.

It is still one Rust binary: LLM providers (Anthropic, OpenAI, Ollama, and ~20 others), 30+ channels (Discord, Telegram, Matrix, email, voice, webhooks, CLI), and tools (shell, browser, HTTP, hardware, custom MCP servers). Everything runs on your machine, with your keys, in your workspace.

What this fork adds on top of upstream ZeroClaw 0.8.6. Inherited runtime behavior is under [What the runtime does](#what-the-runtime-does).

## What ZeroLite adds

### Context engine

`zeroclaw-context` sets the send budget and keeps the prefix cacheable. Policy is on the runtime profile:

```toml
[runtime_profiles.coder.context]
request_token_ceiling = 100_000      # optional window ceiling
trim_threshold_percent = 80     # default water-line
reserve_tokens = 16_384         # output headroom
keep_recent_turns = 5           # default, clamped 1–10
```

- Past the water-line, keep the newest whole turns, then drop older ones down to one. A turn is never split. If the newest turn still does not fit, the turn fails.
- A trim rewrites stored history and refreshes the meter. Size is a script-aware estimate, then re-anchored on provider `input_tokens`.
- Sends keep the longest byte-stable prefix. Cache hits are `zeroclaw_tokens_cached_input_total` and the composer's cache-hit percent. A child's context budget cannot exceed its parent's.
- Stable prefix: no dates or clocks in the system prompt, channel turn context, or history. `HEARTBEAT.md` loads only for heartbeat. Skill and deferred catalogs are frozen for the session and sorted by name.

[Context management](docs/book/src/agents/context-management.md).

### Web chat

- Context ring, plus `N turns · M steps · … tok · cache hit …%`.
- A turn is ordered steps. Earlier steps collapse (`7 tool calls · 6 messages`); the answer stays open. The transcript stays pinned, and a trim cuts it in place.
- An `https://` markdown image renders in the bubble and in tool steps. Publish the picture to an image host first.
- Denser chrome, message hover actions, and tall images scroll.

### Sessions, pairing, delegates

- `session_search` finds this agent's messages. `sessions_history` can read around a hit (`around_message_id`).
- `gateway.master_pair_code` mints a device token without using up the one-time pairing code. It is a secret, and it does nothing while pairing is off. Details live in the `gateway.master_pair_code` config key.
- Nested background delegates stay visible to their caller.

## Install

Install from the [ZeroLite 1.2.2 release](https://github.com/iintothewind/zerolite/releases/tag/v1.2.2). Each binary has the web dashboard embedded and carries the WASM plugin host. There is no macOS build.

| Platform | Asset |
|---|---|
| Windows x64 | `zerolite-1.2.2-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `zerolite-1.2.2-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `zerolite-1.2.2-aarch64-unknown-linux-gnu.tar.gz` |

Linux x64:

```sh
gh release download v1.2.2 -R iintothewind/zerolite -p 'zerolite-1.2.2-x86_64-unknown-linux-gnu.tar.gz'
tar -xzf zerolite-1.2.2-x86_64-unknown-linux-gnu.tar.gz
install -m 755 zeroclaw "${CARGO_HOME:-$HOME/.cargo}/bin/zeroclaw"
zeroclaw quickstart
```

Linux arm64: same steps, with `zerolite-1.2.2-aarch64-unknown-linux-gnu.tar.gz`.

Windows (PowerShell):

```powershell
gh release download v1.2.2 -R iintothewind/zerolite -p 'zerolite-1.2.2-x86_64-pc-windows-msvc.zip'
Expand-Archive .\zerolite-1.2.2-x86_64-pc-windows-msvc.zip -DestinationPath "$env:USERPROFILE\.cargo\bin"
Rename-Item "$env:USERPROFILE\.cargo\bin\zerolite-1.2.2-x86_64-pc-windows-msvc.exe" zeroclaw.exe
zeroclaw quickstart
```

## Quick start

```bash
zeroclaw quickstart               # one-shot setup: pick a provider, write a working config
zeroclaw agent -a <alias>         # interactive chat using the [agents.<alias>] entry
zeroclaw service install          # register as systemd/launchctl/Windows Service
zeroclaw service start            # run it always-on in the background
```

Full walkthrough: `zeroclaw quickstart` — or pass `--yolo` to skip the safety gates on dev boxes.

## Plugins

The release binaries carry the WASM plugin host, so channel and tool packages
install by name — no Rust toolchain needed:

```bash
zeroclaw plugin search discord     # look in the registry
zeroclaw plugin install discord    # install, load-check, and bind a channel
zeroclaw plugin list --verify      # installed packages and their load verdict
```

The registry offers 13 packages built for upstream v0.8.6's plugin contract:
the Discord, Matrix, Telegram, Bluesky, Gitea, Mattermost, Mochat, Notion,
Reddit, X (Twitter), WeChat and WeCom channels, plus the `redact-text` tool.
Plugins stay disabled until you enable them. Operator guide:
[Plugins](docs/book/src/plugins/index.md) · release notes:
[docs/releases/v1.2.0.md](docs/releases/v1.2.0.md).

## What the runtime does

- **Multi-channel** — one agent answering you across every channel you configure (Discord, Telegram, Matrix, email, webhooks, CLI). All delivered to the same agent loop.
- **Provider-agnostic** — model providers are pluggable. Configure Anthropic, OpenAI, local Ollama, or any OpenAI-compatible endpoint. Fallback chains and routing keep the agent running when a provider flakes.
- **Security-first, with escape hatches** — default autonomy is `supervised`: medium-risk ops require approval, high-risk blocked. Workspace boundaries, command policy, OS-level sandboxes (Landlock / Bubblewrap / Seatbelt / Docker), and cryptographic tool receipts on every action. `--yolo` exists for trusted dev environments.
- **Hardware-capable** — GPIO / I2C / SPI / USB on Raspberry Pi, STM32, Arduino, and ESP32 via the `Peripheral` trait.
- **Gateway + dashboard** — HTTP / WebSocket gateway for clients, with a web dashboard for chat, memory browsing, config editing, cron management, and tool inspection.
- **SOP engine** — event-triggered Standard Operating Procedures (MQTT / webhook / cron / peripheral) with approval gates and resumable runs.
- **ACP** — IDE / editor integration via Agent Client Protocol (JSON-RPC 2.0 over stdio).

## Configuration

One TOML file at `~/.zeroclaw/config.toml`. Pointers:

- `providers.models.<type>.<alias>` — the universal provider schema
- `channels.<type>.<alias>` blocks — per-channel settings
- `permission_profiles` / sandboxing / tool receipts — the security surface
- [Full config reference](https://docs.zeroclaw.com/master/en/reference/config.html) — generated from the live schema; every key documented

A V3 config has at minimum four section headers (`<type>.<alias>` shaped) — a provider entry, an agent that references it, and a risk profile the agent gates against.

For standard OpenAI Codex subscription auth, Quickstart can write the provider
entry for you:

```bash
zeroclaw auth login --model-provider openai-codex --import ~/.codex/auth.json  # if already signed in with Codex CLI
zeroclaw quickstart --model-provider openai-codex --model gpt-5.4
```

The provider entry uses the canonical OpenAI shape; the alias below is an
example:

```toml
[providers.models.openai.coding]   # type = openai; alias = coding (you choose)
model = "gpt-5.4"
wire_api = "responses"
requires_openai_auth = true
```

…and point your agent at it with `model_provider = "openai.<alias>"`.

Notes:

- Normal OpenAI Codex subscription auth uses stored auth profiles, not an `api_key` on the provider entry.
- Claude Max setup-token auth stays on the canonical Anthropic slot: run `claude setup-token`, choose `setup_token` in Quickstart, and paste the generated token into the API key/token prompt.
- Only set `api_key` / `uri` on `[providers.models.openai.<alias>]` when intentionally targeting a custom OpenAI-compatible gateway or endpoint.
- If you see `provider streaming failed, falling back to non-streaming chat`, ZeroClaw retries the same request in non-streaming mode. Check `zeroclaw auth status` before changing provider config.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│            channels       gateway        ACP                 │
│          (30+ adapters)   (REST/WS)    (JSON-RPC)            │
│                        ↓                                     │
│                   ZeroClaw runtime                           │
│         ┌──────────┬──────────┬──────────┐                   │
│         │  agent   │ security │   SOP    │                   │
│         │   loop   │  policy  │  engine  │                   │
│         └──────────┴──────────┴──────────┘                   │
│              ↓          ↓           ↓                        │
│          providers    tools      memory                      │
│         (Anthropic,  (shell,    (SQLite,                     │
│          OpenAI,     browser,    embeddings)                 │
│          Ollama,     HTTP,                                   │
│          ~20 more)   hardware)                               │
└──────────────────────────────────────────────────────────────┘
```

## License

Dual-licensed: [MIT](LICENSE-MIT) OR [Apache 2.0](LICENSE-APACHE). You may choose either.
