# Delegation and turn semantics

> **Status: specification.** This page records ZeroLite's agent turn and
> delegation behaviors. For the merge rule, see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
> `delegation-turn #N` in merge reports.

## Decision record: why this shape

- **R1 — A caller must be able to see its delegates.** Nested background
  delegates stay visible to their caller, and foreground subagent tasks
  are inspectable. A delegation that disappears from the caller's view
  cannot be steered, cancelled, or accounted for.
- **R2 — Cancellation propagates to the whole subtree.** Cancelling a
  turn cancels its parallel and agentic delegate children with it. Orphan
  delegates that outlive their turn are a resource and correctness bug,
  not an acceptable leftover.
- **R3 — Settlement survives worker exit.** A delegate settlement is
  recovered after its worker exits instead of being lost with the worker.
  Turn accounting must not depend on the worker staying alive.
- **R4 — Peer turns are durable, not in-memory gossip.** Peer-agent inbox
  turns persist, with a bounded lifecycle, so a peer turn is not lost to a
  restart and cannot grow unbounded.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | Delegates tracked per-worker; nested background delegates may be invisible to the caller | Nested background delegates remain visible to their caller; foreground subagent tasks inspectable in zerocode | R1 |
| 2 | Turn cancellation does not reliably reach parallel/agentic delegate children | Cancellation propagates to the whole delegate subtree with the turn | R2 |
| 3 | Delegate settlement tied to worker lifetime | Settlement recovered after worker exit | R3 |
| 4 | Peer inbox turns transient or unbounded | Peer-agent inbox turns persisted with a bounded lifecycle | R4 |

## Merge notes

- **Verify post-merge by scenario, not by diff:** cancel a turn with a
  live parallel delegate and confirm the child stops; kill a worker
  mid-settlement and confirm the settlement is recovered; restart with a
  pending peer turn and confirm it is still there and bounded.
- **Upstream runtime/turn refactors are semantic changes here even when
  they are framed as cleanups.** If a refactor moves turn ownership, it
  must preserve R1–R4 or be reported as new semantics.
- **Code anchors:** `crates/zeroclaw-runtime/src/agent/` (turn, delegate
  settlement, peer inbox), `crates/zeroclaw-tools/src/` (peer send).

## Sources

- Fork commits `176e5b534` (nested background delegates visible to
  caller), `101746055` (recover delegate settlement after worker exit),
  `124e58ee3` (cancel turn parallel and agentic delegate children),
  `6274772bd` (persist peer-agent inbox turns), `5f1ec801f` (inspect
  foreground subagent tasks).
