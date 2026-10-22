# Fork identity and release

> **Status: specification.** This page records the decisions that make
> ZeroLite a distinct release rather than an upstream build with patches.
> For the merge rule, see `docs/merge_update_rules.md`. Decisions
> here are precedent: cite them as `fork-identity #N` in merge reports.

## Decision record

- **R1 — The fork ships under its own name and version.** Release names,
  README, and version are zerolite's (`1.x` line, `docs/releases/` as
  the release-body source). An upstream release-notes or version-bump
  change must never overwrite the fork's.
- **R2 — Upstream automation that assumes upstream is pruned, not
  adapted.** Upstream workflows, release-workflow parity checks, and
  unused upstream tooling are stripped from the fork. A merge that
  restores them reintroduces automation the fork deliberately does not
  run.
- **R3 — The docs site is not part of the fork.** The docs site and the
  translations submodule were dropped; `docs/book/` remains as content,
  but the site build is not a fork deliverable. A merge that restores
  site tooling conflicts with this decision.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | Upstream release names, version, README | Zerolite release names, README, `1.x` version, `docs/releases/<version>.md` as release body | R1 |
| 2 | Upstream CI workflows incl. sync-upstream automation | Workflows stripped; sync-upstream on manual dispatch only; release-workflow parity checks skipped when the workflow is absent | R2 |
| 3 | Docs site + translations submodule | Dropped; book content kept, site build not a deliverable | R3 |

## Merge notes

- **Verify post-merge by inspection:** `Cargo.toml` version and README
  still zerolite's; `.github/workflows/` contains no restored upstream
  automation; no docs-site tooling reappears.
- **Release procedure is not in this doc:** see
  `docs/maintainers/build-cli-binaries.md` and
  `docs/maintainers/release-attestation-runbook.md`.
- **Code anchors:** `Cargo.toml`, `README.md`, `.github/workflows/`,
  `docs/releases/`, `docs/maintainers/`.

## Sources

- Fork commits `24c3601c1` (1.0.0 readme and release names),
  `8022679ed` (fork versioning), `c35cb26e8` (drop unused upstream
  tooling), `81ddd2e7c` (strip upstream workflows),
  `2bdc9a951` (drop docs site and translations submodule),
  `78c0be38b` (release body from `docs/releases/<version>.md`).
