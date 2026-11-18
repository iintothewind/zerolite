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
- **R4 — Upstream distribution and community assets are stripped, not
  carried.** The fork ships only the three CLI archives built by
  `release-cli.yml`. What upstream keeps for its other channels is deleted:
  container images (root `Dockerfile*`, `Containerfile`, `Makefile`,
  `docker-compose*.yml`, `dev/ci/`, the Docker dev sandbox in `dev/`,
  `apps/zerorelay/Dockerfile`, `apps/zerorelay/compose.yaml`, `demo/`),
  package-manager manifests (`dist/aur`, `dist/scoop`, `dist/freebsd`),
  installers (`install.sh`, `setup.bat`), the desktop app (`apps/tauri`,
  `scripts/desktop/`), `.github/` community configuration (everything
  outside `.github/workflows/`), upstream CI, release and triage scripts
  (`scripts/ci/` except the plugin smoke set, `scripts/release/`,
  `scripts/github/`, `scripts/docs/`), the Raspberry Pi deploy kit in
  `scripts/`, and the `xtask` renderers and architecture detectors that
  exist only to guard those files. An upstream change to a stripped path
  arrives as a modify/delete conflict and is dropped (`git rm`), never
  restored; a new upstream file of the same kind is deleted in the merge.
  In a file the fork keeps, an upstream hunk that re-adds a stripped
  surface (an `xtask` generator registry row, a module declaration) is
  dropped, and a test that reads a stripped file stays behind its
  skip-when-absent guard.

## Where upstream conflicts

| # | Upstream | ZeroLite (this page) | Grounded in |
|---|---|---|---|
| 1 | Upstream release names, version, README | Zerolite release names, README, `1.x` version, `docs/releases/v<version>.md` as release body | R1 |
| 2 | Upstream CI workflows incl. sync-upstream automation | Workflows stripped; sync-upstream on manual dispatch only; release-workflow parity checks skipped when the workflow is absent | R2 |
| 3 | Docs site + translations submodule + `docs/book/` content | Deleted; upstream `docs/book/` edits dropped on modify/delete (`git rm`), never restored | R3 |
| 4 | `zeroclaw update` self-updates from upstream releases, `--standard` switches a CLI-only build to the standard one (#12111) | Self-update, including `--standard`, stays disabled (`fork_self_update_disabled`) | R1, R2 |
| 5 | Containers, package-manager manifests, installers, desktop app, `.github` community config, CI/release/triage scripts, Pi deploy kit, with their `xtask` renderers and architecture detectors | Deleted; upstream edits dropped on modify/delete (`git rm`); hunks that re-add a stripped surface to a kept file dropped | R4 |

## Merge notes

- **Verify post-merge by inspection:** `Cargo.toml` version and README
  still zerolite's; `.github/workflows/` contains no restored upstream
  automation; no docs-site tooling reappears; no stripped distribution or
  community asset reappears (`.github/` holds only `workflows/`;
  `cargo generate installers --check` lists exactly `runtime-locales`,
  `tools-en-ftl`, `zerocode-themes`).
- **Release procedure is not in this doc:** see `docs/maintainers/build-cli-binaries.md` § Release checklist.
- **Code anchors:** `Cargo.toml`, `README.md`, `.github/workflows/`,
  `docs/releases/`, `docs/maintainers/`, `dev/cross/`, `scripts/dev/`.
- **Fork-owned tooling inside upstream directories** (never treat as
  upstream, never drop in a merge): `.github/workflows/release-cli.yml`,
  `cross-image.yml`, `sync-upstream.yml`; `dev/cross/Dockerfile`;
  `scripts/dev/build-cli-local.sh`, `build-cli-local.cmd`,
  `collect-dist.sh`, `reclaim-cargo-disk.sh`, `reclaim-cargo-disk.cmd`,
  `test-image.Dockerfile`.
- **Upstream files that look strippable and are not** (R4 does not cover
  them): `firmware/` (symlinked into `crates/zeroclaw-hardware` and
  `crates/zeroclaw-runtime`, embedded with `include_str!`);
  `dev/config.template.toml` and `dev/config.harness-test.toml` (gateway
  tests, `zeroclaw-config` fixture symlink); `.dockerignore` (build
  context of the `zeroclaw-test` image, `dockerignore_test.rs`);
  `scripts/ci/build_plugin_smoke_packages.sh`, `plugin_artifact_smoke.sh`,
  `plugin_smoke_provider.py` and their two test files (release workflow
  smoke); `apps/zerorelay/relay.example.toml` (embedded by the relay).

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
- v0.9.0 full merge (`merge-v0.9.0`): `docs/book`, workflow, `CLAUDE.md`,
  `CHANGELOG-next.md`, `nix/` and `flake.nix` sides dropped; README and
  version kept; upstream tag never created locally or pushed.
- 2026-11-18 strip of upstream distribution and community assets (R4,
  owner decision): commits `880d6ed3e`, `a9ffa44f0`, `a44828939`,
  `4d2379217`, `a07f277d9`, `fd8ae8a7c`, `51d82d088`, `0fd26526c`.
  `firmware/` kept.
