# Prompt-cache prefix stability

> **Status: specification.** This page defines ZeroLite's prefix-stability
> rules: the parts of the prompt that must be byte-stable within a session
> so provider prompt caching survives across turns. It is the mechanism
> behind `context-management.md` R1. For the merge rule, see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
> `prompt-cache-prefix #N` in merge reports.

## Decision record: why this shape

- **R1 — Cache hits are worth more than prompt elegance.** On local
  inference, re-prefill dominates turn latency. Any change that reorders,
  re-renders, or re-injects prompt content mid-session silently converts
  cached turns into full re-prefills.
- **R2 — Order must be deterministic, not discovery-ordered.** Tool
  specs, skills, and deferred catalog entries are sorted (tool specs by
  name; skills by name; deferred stubs by prefixed name) and frozen for
  the session, so the catalog text is a stable cacheable prefix.
- **R3 — The prefix is fingerprinted, not assumed.** The system and tools
  prefix is fingerprinted on `llm_request` events, so a prefix change is
  observable instead of silently paid for.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | Tool specs, skills, and catalogs in discovery/insertion order | Sorted and session-frozen: tool specs sorted, skills name-sorted, deferred MCP stubs sorted by `prefixed_name` | R2 |
| 2 | No prefix fingerprint on request events | System + tools prefix fingerprinted on `llm_request` events | R3 |
| 3 | Provider sends rebuild the message list from scratch | Provider send path reconciles through an append-only log preserving the longest byte-stable prefix; pruning rewrites only the tail past the first divergent message | R1 (see also `context-management.md` § cache-economics feedback) |

## Merge notes

- **Verify post-merge by behavior:** run two identical turns and
  confirm the provider-reported cached input is non-zero on the second;
  reorder a tool registration and confirm the rendered prompt prefix is
  unchanged.
- **Upstream changes to prompt construction, tool registration order,
  skill injection, or catalog rendering are semantic changes here even
  when they look cosmetic.** A "harmless" reorder is a cache regression.
  If a change is covered by R1–R3, apply the fork behavior (precedent
  lane); if it introduces a new prefix-affecting mechanism, report it.
- **Code anchors:** tool-spec name sort —
  `crates/zeroclaw-runtime/src/agent/turn/tool_specs.rs` (test
  `tool_specs_follow_name_order_not_registration_order`); skill name sort —
  `crates/zeroclaw-runtime/src/skills/mod.rs`; prefix fingerprint
  (`system_sha256`, `tools_sha256` on `llm_request`) —
  `crates/zeroclaw-runtime/src/agent/turn/provider_call.rs`; append-only send
  log — `crates/zeroclaw-context/src/log.rs` (`AppendOnlyLog`).

## Sources

- Fork commits `c978692be` (sort tool specs so the prompt-cache prefix is
  stable), `ec124f5f7` (fingerprint system and tools prefix on
  `llm_request`), `a4029fe3e` (provider sends through AppendOnlyLog).
- Locked decisions in `context-management.md` (no wall-clock injection,
  session-frozen name-sorted catalogs, compact skills).
- Upstream #12177 (MCP tool names sorted on listing) — consistent with R2
  (name-sorted catalogs); taken 2026-11-09.
- Upstream #9579 (built-in `zeroclaw-docs` skill) — not taken (owner decision
  2026-11-12, reverted before the v0.9.0 merge): it adds a prompt entry that
  points at version-pinned upstream docs and changes the skills section when
  `read_skill` availability changes.
