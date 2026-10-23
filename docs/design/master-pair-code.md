# Master pair code (gateway recovery credential)

> **Status: specification.** This page defines the `gateway.master_pair_code`
> recovery credential. For the merge rule, see `docs/merge_update_rules.md`.
> Decisions here are precedent: cite them as
> `master-pair-code #N` in merge reports.

## Decision record: why this shape

- **R1 — The one-time code is unavailable exactly when it is needed.**
  Remote and Docker origins cannot read the startup/rotation code, which
  is when a recovery credential matters. A static, operator-configured
  master code exists for that case and no other.
- **R2 — Narrow scope, no new surface.** Only the gateway pairing
  endpoints change. The pairing page is untouched: it already POSTs
  whatever the operator types to `/api/pair`, so no page change is needed
  to accept the master code.
- **R3 — A master code must work on every gateway path, or it is not a
  recovery credential.** The configured code must reach the daemon-hosted
  gateway as well as the standalone gateway. A backend that accepts the
  code while a gateway path does not is a broken recovery story, not a
  partial implementation.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | No static recovery credential; only the one-time startup/rotation code | `gateway.master_pair_code`: when set, submitting it on the pairing page or to `/api/pair` / `/pair` logs the device in without consuming the one-time code | R1 |
| 2 | — (no equivalent) | The configured master code is carried into the daemon-hosted gateway; the CLI paircode box is sized to the code (30 chars), not to a hard-coded 14-column border | R3; the box sizing is a regression-guarded display fix, not a behavior change |

## Merge notes

- **Verify post-merge end to end, not just in Rust.** This feature's
  defining failure mode is a backend that passes its unit tests while a
  gateway path still rejects the code — that is exactly what the last
  stabilization fix corrected. Post-merge verification must exercise:
  (a) the standalone gateway pairing page with the master code,
  (b) the daemon-hosted gateway with the master code,
  (c) the one-time code still working and still being consumed.
  Unit tests passing alone do **not** close this feature.
- **Tests:** `crates/zeroclaw-config/tests/master_pair_code.rs` and the
  paircode-box width test are fork tests; they must survive the merge.
  Any upstream change to pairing endpoints, daemon gateway hosting, or
  config schema that touches this path needs a new end-to-end check, not
  just a unit-test re-run.
- **Code anchors:** `crates/zeroclaw-config/src/pairing.rs`,
  `crates/zeroclaw-config/src/schema.rs` (`GatewayConfig`),
  `crates/zeroclaw-gateway/src/api_pairing.rs`,
  `crates/zeroclaw-runtime/src/daemon/mod.rs`.

## Sources

- Fork commits `1e884d2e2` (master_pair_code + paircode box fix),
  `5f8567576` (carry the configured master code into the daemon-hosted
  gateway).
