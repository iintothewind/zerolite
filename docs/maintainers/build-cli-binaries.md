# Release Runbook: CLI Binaries (fork-local)

**Workflow:** `.github/workflows/release-cli.yml` (`Release CLI`).
**Publishes:** the `zeroclaw` CLI for Windows x64, Linux x64 and Linux arm64 as GitHub Release assets on `iintothewind/zerolite`.
**Not upstream's pipeline:** it omits feature resolution (xtask), SBOMs, attestations, crates.io / Docker / package-manager publishing, desktop builds and notifications, all of which need upstream repo secrets a fork does not have.

## Release checklist

Run from a clean `zerolite` checkout level with `origin/zerolite`. `X.Y.Z` is the new version.

1. **Bump the version.** In `Cargo.toml`, set `[workspace.package] version` and the 26 `[workspace.dependencies]` path pins (`crates/*`, `apps/*`) to `X.Y.Z`: 27 lines. Refresh the lock with `cargo update --workspace --offline`. `git diff --numstat Cargo.lock` must read `32 32` (the 32 workspace members, version lines only) and `cargo metadata --locked --offline --format-version 1 >/dev/null` must succeed.
2. **Verify** under the `AGENTS.md` test rules (`zeroclaw-test` image, one command at a time, `-- --test-threads=1`): the tests of the crates and modules changed since the last tag, `cargo fmt --all --check`, clippy on the touched crates, `cargo build --locked --features embedded-web,plugins-wasm-cranelift --bin zeroclaw`; for the dashboard, `npm --prefix web test` and `npm --prefix web run build`.
3. **Write `docs/releases/vX.Y.Z.md`** and delete the previous release's file; the repository keeps only the notes of the release being cut. Order: summary (upstream base, config/store migration statement), `## Before you upgrade` when an operator must act, one section per change area, `## Install` (asset table), `## Build notes` (what was verified, with counts).
   The first release after the v0.9.0 merge must carry, under `## Before you upgrade`: run `zeroclaw config migrate` once (config schema version 4).
4. **Commit** the three items above as `chore(release): prepare X.Y.Z` and push `zerolite`.
5. **Tag the pushed commit and push the tag:**

   ```bash
   git tag -a vX.Y.Z -m "ZeroLite X.Y.Z"
   git push origin vX.Y.Z
   ```

6. **Check the run and the release** (a run takes 30–40 minutes):

   ```bash
   gh run list -R iintothewind/zerolite --workflow release-cli.yml --limit 1
   gh release view vX.Y.Z -R iintothewind/zerolite --json name,assets -q '.name, .assets[].name'
   ```

   Expect `ZeroLite X.Y.Z` and the three assets under [What gets built](#what-gets-built).

Do not use `scripts/release/bump-version.sh` or `scripts/release/cut_release_tag.sh`. They are upstream tooling: the first also regenerates upstream install surfaces (`cargo generate installers`), the second requires `HEAD == origin/master`.

## Trigger

**Tag push.** Filter `v[0-9]+.[0-9]+.[0-9]+`: strict semver, so `-rc` / `-canary` suffixes do **not** match. The workflow does not compare the tag against `Cargo.toml`.

The run uses the workflow definition stored **in the tagged commit**. `release-cli.yml` exists only on `zerolite`, so a tag that originates upstream (an `upstream/master` merge, GitHub's "Sync fork") never points at a commit carrying the file and never triggers a build.

**Manual.** Actions → Release CLI → Run workflow → branch `zerolite` → `version` (e.g. `1.3.3`). Same packaging path; the release step creates the tag from the entered version. A run for an existing tag updates that release in place, notes and assets included, which is the cheapest way to repair a bad publish.

## What gets built

| Platform | Target | Runner | Asset |
|---|---|---|---|
| Windows x64 | `x86_64-pc-windows-msvc` | `windows-latest` | `zerolite-<version>-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `zerolite-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `ubuntu-22.04` | `zerolite-<version>-aarch64-unknown-linux-gnu.tar.gz` |

Jobs: `web` (dashboard assets) → `build` (three-way matrix, `--features embedded-web,plugins-wasm-cranelift`, so `web/dist` is compiled in through `include_dir!` and the WASM plugin host is included) → `release` (attach assets).

- Each `build` leg runs a **plugin host smoke test** before packaging: `scripts/ci/build_plugin_smoke_packages.sh` builds the `wasm32-wasip2` fixture packages and `scripts/ci/plugin_artifact_smoke.sh` installs and exercises them against the fresh binary. A binary whose plugin host cannot load a real package fails the leg and is never published. The aarch64 leg runs the smoke inside a `linux/arm64` container.
- The `release` job checks out the repo **before** downloading artifacts: `actions/checkout` defaults to `clean: true`, which would delete archives downloaded ahead of it.
- **Release notes:** the body is `docs/releases/v<version>.md` when that file exists in the run's checkout; otherwise GitHub generates notes from the commit range (`No release notes file` in the `Resolve release notes` step). The title is `ZeroLite <version>`.
- **Linux runners stay on `ubuntu-22.04`.** The arm64 binary is cross-compiled with the Debian cross toolchain (`gcc-aarch64-linux-gnu`) and links glibc dynamically, so it inherits the runner's symbol floor: `GLIBC_2.34` (verified on a locally built artifact from the same recipe), which Raspberry Pi OS (glibc 2.36+) and Ubuntu 24.04 (2.39) satisfy. `ubuntu-latest` would raise the floor.

## Pits

- **`embedded-web` is not a Cargo default feature.** A plain `cargo build --bin zeroclaw` leaves the dashboard on disk only; replacing the binary on a device does not update the WebUI. Local builds go through `scripts/dev/build-cli-local.sh` ([`build-cli-local.md`](./build-cli-local.md)).
- **`plugins-wasm` is not a default feature either**, so a plain build has no `zeroclaw plugin`. With Cranelift, registry `.wasm` packages are JIT-compiled on load; without it the host can only deserialize host-native `.cwasm`. Cranelift costs roughly +23% binary size and is unavailable on ARM32, which these targets do not cover.
- **`crates/zeroclaw-plugins/wit` is a git symlink** to the repo-root `wit/`. On Windows with `core.symlinks=false` it is checked out as a 9-byte text file and every `plugins-wasm` build fails with `failed to read path for wit/v0`. Fix locally with `cp -r wit crates/zeroclaw-plugins/wit`; GitHub runners check out symlinks correctly.

## Retrieving and deploying an asset

Assets live on the GitHub Release; workflow artifacts are kept for 1 day only.

```bash
gh release list -R iintothewind/zerolite

# On the target host: fetch the latest release's arm64 asset and install it
gh release download -R iintothewind/zerolite \
  -p 'zerolite-*-aarch64-unknown-linux-gnu.tar.gz' -D /tmp
mkdir -p ~/.cargo/bin
tar -xzf /tmp/zerolite-*-aarch64-unknown-linux-gnu.tar.gz -C ~/.cargo/bin
```

Install where `which zeroclaw` resolves, typically `~/.cargo/bin/zeroclaw`, **not** `/usr/local/bin` (PATH order shadows it). Stop the running instance first (`systemctl stop zeroclaw` when service-managed; `kill` alone lets systemd restart it). A later `cargo install` or on-host `cargo build` silently replaces `~/.cargo/bin/zeroclaw` with whatever the host's source tree contains.

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| Workflow never starts on a tag push | Tag does not match `v[0-9]+.[0-9]+.[0-9]+` (e.g. `v1.3.3-rc1`), or the tagged commit predates `release-cli.yml` | Use a plain semver tag; tag a commit that contains the workflow file |
| Release body is auto-generated | `docs/releases/v<version>.md` is missing from the tagged commit | Add the file, then re-run through the manual trigger for the same version |
| `web` job fails | `web/dist` must exist before the Rust build; `cargo web build` renders the OpenAPI spec and runs `openapi-typescript` first | Do not replace it with `cd web && npm run build` |
| WebUI unchanged on device after installing the release binary | Build omitted `--features embedded-web` (not in Cargo defaults) | Workflow must pass `embedded-web`; locally use `scripts/dev/build-cli-local.sh` |
| arm64 binary fails on target with `GLIBC_x.y not found` | Target host glibc older than the runner's symbol floor | Verify with `readelf -V <binary>`; do not raise the runner version |
| arm64 link error `Relocations in generic ELF (EM: 183)` | Wrong-architecture C toolchain selected (e.g. a mislabelled cross image) | Confirm `aarch64-linux-gnu-gcc` is installed and `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` is exported |
