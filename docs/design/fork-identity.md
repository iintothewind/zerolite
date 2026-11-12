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
  run. An upstream change to a stripped workflow (e.g.
  `.github/workflows/ci.yml`) arrives as a modify/delete conflict and
  its workflow side is dropped (`git rm`), never restored.
- **R3 — The docs site is not part of the fork.** The docs site, its
  translations submodule, and the `docs/book/` content are deleted. An
  upstream commit that modifies a `docs/book/` file arrives as a
  modify/delete conflict, and its docs side is dropped (`git rm`), never
  restored.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | Upstream release names, version, README | Zerolite release names, README, `1.x` version, `docs/releases/v<version>.md` as release body | R1 |
| 2 | Upstream CI workflows incl. sync-upstream automation | Workflows stripped; sync-upstream on manual dispatch only; release-workflow parity checks skipped when the workflow is absent | R2 |
| 3 | Docs site + translations submodule + `docs/book/` content | Deleted; upstream `docs/book/` edits dropped on modify/delete (`git rm`), never restored | R3 |

## Merge notes

- **Verify post-merge by inspection:** `Cargo.toml` version and README
  still zerolite's; `.github/workflows/` contains no restored upstream
  automation; no docs-site tooling reappears.
- **Release procedure is not in this doc:** see `docs/maintainers/build-cli-binaries.md` § Release checklist.
- **Code anchors:** `Cargo.toml`, `README.md`, `.github/workflows/`,
  `docs/releases/`, `docs/maintainers/`.

## Sources

- Fork commits `24c3601c1` (1.0.0 readme and release names),
  `8022679ed` (fork versioning), `c35cb26e8` (drop unused upstream
  tooling), `81ddd2e7c` (strip upstream workflows),
  `2bdc9a951` (drop docs site and translations submodule),
  `78c0be38b` (release body from `docs/releases/v<version>.md`).
- v0.8.7 cherry-picks (`merge-v0.8.7`): `docs/book` and `ci.yml` sides
  dropped.
- 2026-11-09 cherry-picks (`merge-2026-11-09`, release 1.3.1): `docs/book`
  sides dropped (#11855, #11523, #12114).
