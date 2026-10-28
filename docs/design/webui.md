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
  disagree with the runtime. Every frame that prices the request goes
  through `contexts/contextMeter.logic.ts`; a handler added anywhere else
  is a second arithmetic, and a trim would then show up in the transcript
  without moving the meter.
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

### Composer stats semantics (behavior #3)

The InputBar toolbar row renders `N turns · M steps · cache hit …% · X tok/s`
from a single published `LiveStats` snapshot
(`web/src/lib/sessionStats.logic.ts`):

- **Estimator, not timer.** `X tok/s` is the reading of
  `web/src/lib/tokenRate.ts` (`TokenRateMeter`): a decayed sliding window over
  streamed text — three exponential half-lives (5 s / 20 s / 80 s), 250 ms
  buckets — plus the history billed by `usage` frames. The meter is pure and
  timer-free; `rate(now)` computes from arrival timestamps.
- **Lower bound.** The gateway streams answer text only, so a live reading
  counts streamed evidence; the model's unstreamed output (reasoning,
  tool-call arguments) is invisible, so a reading is a lower bound on the true
  speed. The first `usage` frame marks the reading billed: it adds the
  provider's `output_tokens` against the silent span and clears the `est`
  flag.
- **Publisher.** `chunk`/`thinking` frames feed the meter and never publish.
  A 1 Hz ticker publishes the reading while a turn is active, which is what
  lets a stall fall visibly; terminal frames (`message`/`done`/`aborted`)
  publish once and mute it, so the last reading stays on screen, dimmed.
- **No placeholder.** While the meter has no evidence (gate: ≥200 estimated
  tokens over ≥4 s of stream), the segment is absent — never `0 tok/s`.
  Formatting: `formatRate`, integers at ≥10, one decimal below.
- **Reset with the transcript.** Session switch, `clearAllMessages`,
  `history_trimmed`, and unmount zero the meter. The outgoing conversation's
  speed must not follow into the new one.
- The cumulative `X tok` segment is retired; session totals live in the
  session dashboard.

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
