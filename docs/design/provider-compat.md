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
| 7 | Tool-result images stay live for the current user turn (not inlined base64 into screenshots). Since v0.9.0 (#10502, undecided) OpenAI-compatible endpoints relocate tool-result images into a user message at the wire by default | `fix(multimodal)`, `fix(tools): stop inlining base64 into screenshot results` |
| 8 | Shell output encoding detection; PowerShell output initialized as UTF-8 | `fix(runtime): detect encoding for shell output`, `feat(shell): initialize PowerShell output as UTF-8` |
| 9 | A chat-completions reply cut at the output limit (`finish_reason: "length"`) is reported, not passed off as finished: WARN `provider_output_truncated`, `StreamEvent::OutputTruncated`, WARN `turn_output_truncated`, and a display-only `output_truncated` WS frame for a text reply | `fix(providers): report replies cut at the output limit` |
| 10 | Vision routing counts only the markers this turn introduces (`count_current_turn_image_markers`); images carried over from earlier turns are stripped for a non-vision primary and never move the turn onto `vision_model_provider`. Holds for the skill-review fork too: its review prompt makes the reviewed image carried over (owner ruling 2026-11-12, v0.9.0 merge; upstream's `skill_review_routes_vision_through_the_turns_binding` became `skill_review_keeps_carried_over_images_off_the_vision_route`) | `dadf41301` `fix(runtime): carried-over images keep the selected model`; test `carried_over_images_keep_the_selected_model` |
| 11 | DeepSeek endpoints (`deepseek` family, or host `api.deepseek.com`) always receive the configured `reasoning_effort`; `xhigh` is sent as `max` | `deepseek_contract`, `reasoning_effort_for_model`; test `deepseek_contract_forwards_reasoning_effort_and_sends_max_for_xhigh` |
| 12 | DeepSeek thinking toggle: alias `think`, else `[runtime] reasoning_enabled`, is sent as `thinking: {"type": "enabled" \| "disabled"}`; an operator `provider_extra.thinking` wins | `OpenAiCompatibleBuilder::thinking_enabled`; test `deepseek_thinking_toggle_is_merged_under_configured_extras` |
| 13 | DeepSeek assistant tool-call history always carries `reasoning_content` (empty string when none is stored) | `convert_messages_for_native`; test `deepseek_tool_call_history_always_carries_reasoning_content` |
| 14 | Chat-completions `finish_reason` `insufficient_system_resource` / `aborted` is a provider error, not a finished reply; partial tool calls are dropped | `INTERRUPTED_FINISH_REASONS`; tests `interrupted_finish_reasons_end_the_stream_with_an_error`, `interrupted_finish_reason_fails_a_non_streaming_reply` |
| 15 | DeepSeek context window is discovered from `GET /models` (`context_window`); no built-in table entry | `SERVES_MODEL_CONTEXT_CATALOG` on `DeepseekModelProviderConfig` |
| 16 | `web_search` route `deepseek` (DeepSeek Search API, `[web_search] deepseek_api_key`) | `crates/zeroclaw-tools/src/web_search_tool.rs` `search_deepseek` |
| 17 | `custom` aliases send their own `think` as `chat_template_kwargs.enable_thinking` (chat-template backends behind LiteLLM ignore a top-level `enable_thinking`); an operator `chat_template_kwargs.enable_thinking` wins; `[runtime] reasoning_enabled` is not mapped; skipped under `thinking_passthrough` | `OpenAiCompatibleBuilder::chat_template_thinking`; test `custom_think_reaches_chat_template_kwargs` |
| 18 | `custom` aliases probe `GET {uri}/models` for a context window; LiteLLM's `max_input_tokens` is read after `context_length` and `context_window` | `MODEL_CONTEXT_CATALOG_AUTH` on `CustomModelProviderConfig`; tests `custom_reads_litellm_max_input_tokens`, `custom_probes_with_the_same_auth_its_request_path_uses` |
| 19 | Per-turn reasoning effort: a surface scopes `REASONING_EFFORT_OVERRIDE` with `REASONING_EFFORT_REJECTED` (`scope_reasoning_effort`; web chat `effort`; none scoped = configured behavior), and each provider maps it for the levels its `reasoning_support` lists. A level the serving model does not list is ignored. Anthropic 4.7+: adaptive `thinking` plus `output_config.effort`; older Claude (also on Bedrock): `low`/`medium`/`high` as 2,048/8,192/16,384 thinking-budget tokens; OpenAI Responses, Codex, Azure: `reasoning_effort`; DeepSeek: `reasoning_effort` verbatim plus the `thinking` toggle. Any other OpenAI-compatible model is assumed to take every level as `reasoning_effort` (`off` → `"none"`, or `chat_template_kwargs.enable_thinking: false` on a chat-template `think` alias); alias key `reasoning_levels` replaces the assumption. When a compatible endpoint answers such a request with an error status other than 401/403/429, it is retried once without the effort, the rest of the turn runs without it, and the surface is told (`reasoning.rejected`); nothing is remembered past the turn | `crates/zeroclaw-providers/src/reasoning.rs`, `ModelProvider::reasoning_support`, `drop_rejected_effort`; tests named `reasoning_*` in each provider |

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
- **DeepSeek (rows 11–16):** behaviors were probed live on 2026-11-20
  against `deepseek-flash`: `reasoning_effort` accepts
  `none|minimal|low|medium|high|xhigh|ultra|max`; `thinking.type`
  overrides `reasoning_effort`; `enable_thinking` is ignored; a tool-call
  assistant message without `reasoning_content` is HTTP 400 in thinking
  mode unless DeepSeek produced the call in thinking mode, and
  `reasoning_content: ""` is accepted.
- **Qwen open-weight (rows 17–18):** probed live on 2026-11-20 through
  a LiteLLM proxy (aliases on Strata 0.1.62 with qwen3.8-flash, vLLM
  0.29.0 with Qwen3.5-9B, hosted Qwen3.8-27B with the official template).
  A top-level `enable_thinking: false` is ignored on every alias;
  `chat_template_kwargs: {"enable_thinking": false}` turns thinking off,
  and LiteLLM forwards `chat_template_kwargs` unchanged.
  `chat_template_kwargs.preserve_thinking` and `.reasoning_effort` also
  reach the template. The official Qwen3.8 template answers HTTP 400 for
  any effort other than `low|medium|xhigh`, top-level or inside the
  kwargs, so operators set `chat_template_kwargs.reasoning_effort` per
  alias and do not enable `reasoning_effort_passthrough` with a global
  `high`. Qwen3.6 defaults `preserve_thinking` off and Qwen3.8 defaults it
  on; set it per alias in `chat_template_kwargs`. History reasoning
  replayed as `reasoning_content` is accepted on all aliases (LiteLLM
  renames vLLM's `reasoning` to `reasoning_content` in responses);
  reasoning replay, streaming, usage and parallel tool calls need no
  shim. LiteLLM `GET /v1/models` publishes the window only as
  `max_input_tokens`. No Qwen detection by model name and no Qwen family
  shim: the toggle keys off the `custom` family and the alias's `think`.
- **Code anchors:** `crates/zeroclaw-providers/src/`,
  `crates/zeroclaw-runtime/src/agent/turn/` (seam sanitizers, media).

## Sources

- Fork commits listed in the table above (provider, anthropic, multimodal
  areas of the fork commit list).
