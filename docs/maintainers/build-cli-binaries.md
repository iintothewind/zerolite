# Build Runbook: CLI Binaries (fork-local)

**File:** `.github/workflows/release-cli.yml`
**Scope:** publish the `zeroclaw` CLI for Windows x64, Linux x64 and Linux arm64 as GitHub Release assets.
**Relationship to upstream:** this workflow is independent of `release-stable-manual.yml`. It deliberately omits feature resolution (xtask), SBOMs, attestations, crates.io / Docker / package-manager publishing, desktop builds and notifications — all of which require upstream repo secrets a fork does not have.

## Trigger

### Automatic — push a version tag

```bash
git tag v0.8.6
git push fork v0.8.6
```

Filter: `v[0-9]+.[0-9]+.[0-9]+` (strict semver; `-rc` / `-canary` suffixes do **not** match). The workflow does not compare the tag against `Cargo.toml`, so any semver tag is accepted.

The workflow runs against the workflow definition stored **in the tagged commit**. Because `release-cli.yml` exists only on this branch, tags that originate upstream (from `origin/master` merges or GitHub's "Sync fork") never match a commit containing this file, and therefore never trigger a build. Only a tag placed on a commit that already carries `release-cli.yml` will run it.

### Manual — Actions tab

**Actions → Release CLI → Run workflow → branch `zerolite` → enter `version`** (e.g. `0.8.6`). Same packaging path; the tag is created by the release step from the entered version.

Recommended for verification runs and for pre-release builds.

## What gets built

| Platform | Target | Runner | Asset |
|---|---|---|---|
| Windows x64 | `x86_64-pc-windows-msvc` | `windows-latest` | `zerolite-<version>-x86_64-pc-windows-msvc.zip` |
| Linux x64 | `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | `zerolite-<version>-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `ubuntu-22.04` | `zerolite-<version>-aarch64-unknown-linux-gnu.tar.gz` |

Job order: `web` (dashboard assets) → `build` (three-way matrix with
`--features embedded-web,plugins-wasm-cranelift`, so `web/dist` is compiled
into the binary via `include_dir!` and the WASM plugin host is included) →
`release` (attach assets).
Each `build` leg runs a **plugin host smoke test** before packaging:
`scripts/ci/build_plugin_smoke_packages.sh` builds the `wasm32-wasip2` fixture
packages, and `scripts/ci/plugin_artifact_smoke.sh` installs and exercises
them against the freshly built binary. A binary whose plugin host does not
load a real package fails the leg and is never published. The aarch64 leg
runs the smoke inside a `linux/arm64` container because the archive targets a
foreign architecture.

The `release` job checks out the repo **before** downloading artifacts:
`actions/checkout` defaults to `clean: true` and deletes the workspace, which
would remove the archives downloaded ahead of it.

### Release notes

The release body comes from `docs/releases/v<version>.md` when that file exists
in the run's checkout; otherwise GitHub generates notes from the commit range.
The title is `ZeroLite <version>`.

Write the file before tagging, in the tagged commit, or the run silently falls
back to generated notes (`No release notes file` in the `Resolve release notes`
step). A `workflow_dispatch` run for an existing tag updates that release in
place, notes and assets included, which is the cheapest way to repair a bad
publish.

> **Pit:** Cargo `default` features do **not** include `embedded-web`. A plain
> `cargo build --bin zeroclaw` leaves the dashboard on disk only; replacing the
> binary on a Pi will not update the WebUI. Local builds must use
> `scripts/dev/build-cli-local.sh` (see [`build-cli-local.md`](./build-cli-local.md)).

> **Pit:** these archives carry `embedded-web,plugins-wasm-cranelift`. `plugins-wasm` alone is not a default feature, so a plain `cargo build --bin zeroclaw` has no `zeroclaw plugin`. With Cranelift enabled, registry `.wasm` packages are JIT-compiled on load; without it the host can only deserialize host-native `.cwasm`. Cranelift costs roughly +23% binary size and is unavailable on ARM32, which these three targets do not cover.

> **Pit:** `crates/zeroclaw-plugins/wit` is a git symlink to the repo-root `wit/`. On Windows with `core.symlinks=false` it is checked out as a 9-byte text file, and every `plugins-wasm` build then fails with `failed to read path for wit/v0`. Resolve it locally with `cp -r wit crates/zeroclaw-plugins/wit`; GitHub runners check out symlinks correctly.

### glibc compatibility of the arm64 build

The arm64 binary is cross-compiled with the Debian cross toolchain (`gcc-aarch64-linux-gnu`) on ubuntu-22.04 and dynamically links against glibc, so it is not a static binary and inherits the runner's glibc symbol floor. Verified on a locally built artifact from the same recipe: highest required symbol version is `GLIBC_2.34`, which is satisfied by Raspberry Pi OS (glibc 2.36+) and Ubuntu 24.04 (glibc 2.39).

If the workflow is ever moved to `ubuntu-latest`, the glibc floor rises and older target hosts may stop working. Keep `ubuntu-22.04` for the Linux legs.

## Retrieving and deploying an asset

Assets are attached to the GitHub Release; there are no intermediate artifacts worth keeping (retention is 1 day).

```bash
# List the latest release's assets
gh release list -R iintothewind/zeroclaw

# Fetch a single asset directly onto the target host
gh release download v0.8.6 \
  -R iintothewind/zeroclaw \
  -p 'zerolite-*-aarch64-unknown-linux-gnu.tar.gz' \
  -D /tmp

ssh <host> 'mkdir -p ~/.cargo/bin && tar -xzf /tmp/zerolite-*.tar.gz -C ~/.cargo/bin'
```

Deploy to the path `which zeroclaw` actually resolves to — typically `~/.cargo/bin/zeroclaw`, **not** `/usr/local/bin` (PATH order means the latter is shadowed). Stop the running instance first (`systemctl stop zeroclaw` if service-managed; `kill` alone lets systemd restart it).

Overwriting `~/.cargo/bin/zeroclaw` is destructive to a locally built binary: a later `cargo install` or on-host `cargo build` silently replaces it with whatever the host's source tree contains.

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| Workflow never starts on a tag push | Tag does not match `v[0-9]+.[0-9]+.[0-9]+` (e.g. `v0.8.6-rc1`), or the tagged commit predates `release-cli.yml` | Use a plain semver tag; tag the commit that contains the workflow file |
| `web` job fails | `web/dist` must exist before the Rust build; `cargo web build` renders the OpenAPI spec and runs `openapi-typescript` first | Do not replace it with `cd web && npm run build` |
| WebUI unchanged on device after installing the release binary | Build omitted `--features embedded-web` (not in Cargo defaults) | Workflow must pass `embedded-web`; locally use `scripts/dev/build-cli-local.sh` |
| arm64 binary fails on target with `GLIBC_x.y not found` | Target host glibc older than the runner's symbol floor | Verify with `readelf -V <binary>`; do not raise the runner version |
| arm64 link error `Relocations in generic ELF (EM: 183)` | Wrong-architecture C toolchain selected (e.g. a mislabelled cross image) | Confirm `aarch64-linux-gnu-gcc` is installed and `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER` is exported |

## Related

- `docs/maintainers/release-attestation-runbook.md` — upstream release pipeline (secrets-gated, not usable from a fork).
- `docs/book/src/maintainers/ci-and-actions.md` — general CI/Actions overview.
