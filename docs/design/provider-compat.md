# Provider compatibility shims

> **Status: specification.** This page records ZeroLite's provider
> compatibility shims: small, provider-specific behaviors that upstream
> changes must not silently drop. Each shim exists because a real provider
> behaved differently from the spec. For the merge rule, see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
> `provider-compat #N` in merge reports.

## Decision record: why shims are decisions, not hacks

- **R1 — A shim encodes a provider fact.** `reasoning_effort` passthrough,
  an Anthropic cache breakpoint, a Grok frame limit: each is a statement
  about a specific provider's wire behavior. Deleting one because it
  looks redundant reintroduces the provider failure it was written for.
- **R2 — Shims are opt-in and bounded.** Each shim is gated (a flag, a
  provider family, a capability check) and does not change the default
  path for other providers. An upstream change that widens a shim's blast
  radius changes its semantics.

## Shims

| # | Shim | Anchor |
|---|---|---|
| 1 | `reasoning_effort` forwarded through compatible providers via opt-in passthrough flag | `fix(providers): forward reasoning_effort through compatible providers` |
| 2 | Anthropic rolling cache breakpoint kept when the last message ends with an image | `fix(anthropic): keep the rolling cache breakpoint when the last message ends with an image` |
| 3 | Configurable `cache_ttl` for Anthropic prompt-cache markers | `feat(providers): configurable cache_ttl for Anthropic prompt-cache markers` |
| 4 | Outbound Grok ACP frame limits enforced | `feat(providers): enforce outbound Grok ACP frame limits` |
| 5 | Non-object pricing tolerated in OpenAI-compatible model lists | `fix(providers): tolerate non-object pricing in OpenAI-compatible model lists` |
| 6 | Signed reasoning kept intact on the seam sanitizers | `fix(providers): keep signed reasoning intact on the seam sanitizers` |
| 7 | Tool-result images stay live for the current user turn (not inlined base64 into screenshots) | `fix(multimodal)`, `fix(tools): stop inlining base64 into screenshot results` |
| 8 | Shell output encoding detection; PowerShell output initialized as UTF-8 | `fix(runtime): detect encoding for shell output`, `feat(shell): initialize PowerShell output as UTF-8` |
| 9 | A chat-completions reply cut at the output limit (`finish_reason: "length"`) is reported, not passed off as finished: WARN `provider_output_truncated`, `StreamEvent::OutputTruncated`, WARN `turn_output_truncated`, and a display-only `output_truncated` WS frame for a text reply | `fix(providers): report replies cut at the output limit` |

## Merge notes

- **Verify post-merge per shim, by provider behavior where possible.**
  Shims have no shared test file; each is guarded by its own provider
  tests. If an upstream provider refactor deletes or renames a shim's
  tests, that is a conflict signal, not cleanup.
- **Upstream provider changes that remove a shim are new semantics for
  that provider, even when the upstream code compiles.** Report them
  unless a design decision here already covers the removal (none does).
- **Output-limit cut (row 9):** tests
  `length_finish_reason_emits_output_truncated_before_final`,
  `finished_replies_emit_no_output_truncated` (compatible.rs),
  `a_reply_cut_at_the_output_limit_is_announced_and_kept`,
  `a_cut_tool_call_response_and_a_finished_reply_are_not_announced`
  (stream_consume.rs). Only the chat-completions wire maps it today;
  Anthropic `max_tokens`, Gemini `MAX_TOKENS`, OpenAI Responses
  `incomplete` and Ollama `done_reason: length` are not mapped yet.
- **Code anchors:** `crates/zeroclaw-providers/src/`,
  `crates/zeroclaw-runtime/src/agent/turn/` (seam sanitizers, media).

## Sources

- Fork commits listed in the table above (provider, anthropic, multimodal
  areas of the fork commit list).
