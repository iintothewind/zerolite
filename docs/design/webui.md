# Web UI fork behavior

> **Status: specification.** This page records the web UI behaviors that
> are ZeroLite's own and must survive upstream web changes. For the merge
> rule, see `docs/merge_update_rules.md`. Decisions here are
> precedent: cite them as `webui #N` in merge reports.

## Decision record: why this shape

- **R1 — The UI must not re-read what the runtime already told it.**
  On `HistoryTrimmed`, the transcript is cut locally instead of
  re-reading the store. A trim notification carries everything the UI
  needs; a re-read is a round trip that can also race the trim.
- **R2 — The meter and the trim notice are one contract.** The context
  bar and the trim notice render from the same published numbers as the
  runtime (see `context-meter.md`): effective window as denominator,
  trim budget as trigger. A UI that renders its own arithmetic will
  disagree with the runtime.
- **R3 — Fork identity is visible, not just a version string.** The
  zerolite mark is the web logo and the release names are zerolite's.
  Upstream branding assets must not silently replace them in a merge.

## Behaviors

| # | Behavior | Anchor |
|---|---|---|
| 1 | Transcript cut locally on `HistoryTrimmed`, no store re-read | `fix(web): cut the transcript locally on history_trimmed` |
| 2 | Context bar + trim notice from published runtime numbers | `web/src/lib/contextBar.ts`, `historyTrimNotice.ts` (+ tests) |
| 3 | Transcript-aligned composer stats in the InputBar toolbar | `feat(web): put transcript-aligned composer stats in the InputBar toolbar` |
| 4 | Zerolite mark as web logo; zerolite release names | `9548ae163`, `31fb2d06b`, release notes in `docs/releases/` |
| 5 | Zerocode keymap: QueueWiden / QueueNarrow | `apps/zerocode/src/keymap/actions.rs` |

## Merge notes

- **Verify post-merge by behavior:** trigger a trim and confirm the
  transcript updates without a store round trip; confirm the context bar
  denominator equals the published effective window (see
  `context-meter.md`); confirm the logo and release names are still
  zerolite's.
- **Upstream web refactors that touch the transcript store, the context
  bar, or branding assets are semantic changes here.** The web test
  files (`*.test.ts` beside each logic file) are fork tests; they must
  survive the merge.

## Sources

- Fork commits `92d6f1dc0`, `e7b8e2884`, `9548ae163`, `31fb2d06b`,
  and the zerocode keymap actions.
