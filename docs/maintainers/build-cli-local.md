# Build Runbook: CLI Binaries Locally (cross-compile)

**Scope:** build the `zeroclaw` CLI binary **on your own machine** (no GitHub
Actions), primarily to produce a Linux arm64 executable for on-device testing
(Raspberry Pi, Graviton, …).

**Canonical command (use this — do not hand-roll Docker):**

```bash
# Linux / macOS / Git Bash:
bash scripts/dev/build-cli-local.sh
# optional:
# bash scripts/dev/build-cli-local.sh --target x86_64-unknown-linux-gnu

# Windows (PowerShell / cmd) — always Git Bash, never WSL:
scripts\dev\build-cli-local.cmd
```

Features are always `--features embedded-web,plugins-wasm-cranelift`: that is
the release shape, so a plain invocation needs no flags.

The script clears `NODE_OPTIONS` before the web build. Agent shells
(WorkBuddy, Cursor, …) export a `--require` shim there that wraps
`fs.rmSync` in a bulk-delete guard; vite's `emptyOutDir` clears `web/dist`
through `rmSync`, the guard cannot create its lock directory inside the
sandbox, and the build dies with `SAFE_DELETE_BULK_GUARD_ERROR` before writing
anything. That is an environment artifact, not a vite bug — if you invoke
`npm run build` in `web/` yourself, clear `NODE_OPTIONS` first.

`--plugins-wasm` is accepted and is now the **default**; it adds
`plugins-wasm-cranelift`, which gives the binary `zeroclaw plugin`
(search / install / update / …) and lets it JIT-compile registry `.wasm`
packages. `--no-plugins-wasm` opts out, for targets where Cranelift is
unavailable (ARM32) — that is not a supported release shape: without the flag
the binary only loads host-native `.cwasm` packages.

Output (durable — deploy this path only):

| Path | Role |
|---|---|
| `dist/bin/<triple>/zeroclaw` | deploy copy (`collect-dist.sh`) |

The script **always reclaims compile/test residue on EXIT** (success,
failure, or interrupt) via `scripts/dev/reclaim-cargo-disk.sh`:

- repo `target/` (primary multi-hundred-GB offender)
- `%TEMP%\zc-cargo-home`, `%TEMP%\zeroclaw\`, `%TEMP%\zc-*` (Windows agent temps)
- sandbox `CARGO_TARGET_DIR` / `…/cargo-target` when present
- repo `.cache/npm` (Windows web-build workaround)

It does **not** run `docker system prune`. Cargo’s intermediate
`target/<triple>/release/zeroclaw` is gone; the next local build is a cold
start. Keep only `dist/bin/<triple>/zeroclaw`. Default triple:
`aarch64-unknown-linux-gnu`.

After ad-hoc `cargo test` / `cargo check` (outside this script), run the
same reclaim helper — see AGENTS.md item 13.

This is the hands-on counterpart to [`build-cli-binaries.md`](./build-cli-binaries.md)
(CI / GitHub Release). It does not publish anything.

---

## ⚠ The WebUI pit (read this every time)

**Symptom:** you ship a new binary, Rust/behavior changes are live, but the
dashboard still looks like yesterday (ctx bar, Progress, …).

**Root cause (two stacked traps):**

1. **`embedded-web` is NOT in Cargo `default` features.**  
   Plain `cargo build --release --bin zeroclaw` (or the same inside Docker)
   produces a binary that **does not** embed `web/dist`. At runtime the
   gateway serves whatever is on disk at `gateway.web_dist_dir` on the
   device — often a weeks-old tree. Replacing only `~/.cargo/bin/zeroclaw`
   cannot change that UI.

2. **Even with `embedded-web`, `include_dir!` freezes `web/dist` at compile
   time.** A stale `web/dist`, or a gateway crate that was not rebuilt after
   `web/dist` changed, ships the old hashed assets (`AgentChat-XXXX.js`,
   …). Vite content hashes mean "almost the same" is still the wrong UI.

**What the script does so you cannot skip a step:**

1. Wipe `web/dist` (keeps `.gitkeep`)
2. `cargo web build` → fresh OpenAPI + Vite bundle
3. `cargo clean -p zeroclaw-gateway` → force `include_dir!` to re-read dist
4. Docker cross-compile with **`--features
   embedded-web,plugins-wasm-cranelift`** (`--no-plugins-wasm` drops the
   latter)
5. **Assert** the ELF contains the current `assets/index-*.js` fingerprint
   from `web/dist/index.html` (refuse to collect if missing)
6. Copy into `dist/bin/<triple>/` via `scripts/dev/collect-dist.sh`
7. Reclaim compile/test residue on EXIT (`target/`, `%TEMP%\zc-*` / `zc-cargo-home` / `zeroclaw/`, sandbox `cargo-target`)

`--skip-web` exists only when you already rebuilt `web/dist` in this
session and know it is current. Prefer the full pipeline.

**Do not:**

- Hand-copy an old file from `dist/bin/…` without re-running the script
- Run raw `docker … cargo build` without `--features embedded-web`
- Run `cd web && npm run build` instead of `cargo web build` (skips OpenAPI /
  `openapi-typescript`)
- Deploy `target/…/zeroclaw` from a build that never asserted the fingerprint

---

## Prerequisites

| Requirement | Notes |
|---|---|
| Docker (Desktop or Engine) | `docker info` |
| `rust:1-bookworm` image | `docker pull rust:1-bookworm` — Bookworm glibc → `GLIBC_2.34` floor |
| Node.js on the host | For `cargo web build`; pin to `.nvmrc` (currently `24`) |
| Host `cargo` / `npm` | Script runs the web step on the host |

Cross gcc + Rust target are installed **inside** the container.

### Git Bash on Windows

On Windows this script **must** run under Git Bash, not WSL. WSL's `bash` does
not see Windows `cargo.exe` as `cargo`, and a Cursor/sandbox `CARGO_TARGET_DIR`
can make `collect-dist` look in the wrong tree after Docker writes to
`target/<triple>/`.

What the tooling does:

1. `scripts/dev/build-cli-local.cmd` — PowerShell/cmd entrypoint; always launches
   Git Bash's `bash.exe` and clears `CARGO_TARGET_DIR`.
2. `build-cli-local.sh` — if started under WSL (or another non-MSYS bash on
   Windows), re-execs under Git Bash automatically.
3. Host steps `unset CARGO_TARGET_DIR` so collect reads `$REPO_ROOT/target`
   (the tree Docker mounted and wrote into).
4. `MSYS_NO_PATHCONV=1` / `MSYS2_ARG_CONV_EXCL='*'` so `-w /build` is not
   rewritten to a host path.

Prefer:

```bat
scripts\dev\build-cli-local.cmd
```

or, already inside Git Bash:

```bash
bash scripts/dev/build-cli-local.sh
```

---

## What the script runs (summary)

Equivalent intent (the script is authoritative):

```bash
# 1–3 host
rm -rf web/dist/* ; keep .gitkeep
cargo web build
cargo clean -p zeroclaw-gateway

# 4 container (arm64)
docker run --rm -v "$PWD":/build -w /build \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  rust:1-bookworm bash -c '
    set -e
    apt-get update -qq
    apt-get install -y -qq gcc-aarch64-linux-gnu g++-aarch64-linux-gnu
    rustup target add aarch64-unknown-linux-gnu
    cargo build --release --locked \
      --target aarch64-unknown-linux-gnu \
      --features embedded-web,plugins-wasm-cranelift \
      --bin zeroclaw
  '

# 5–7
# strings … | grep assets/index-….js   # must match web/dist/index.html
bash scripts/dev/collect-dist.sh --target aarch64-unknown-linux-gnu --bin zeroclaw
rm -rf target
```

This reference lists both features because that is the default. Pass
`--no-plugins-wasm` to build without the WASM plugin host.

First full run ~10+ minutes. Because the script wipes `target/` after success,
later runs are also cold (no cross-compile crate cache reuse).

---

## Verify

The script already fingerprints the dashboard. Manual checks:

```bash
file dist/bin/aarch64-unknown-linux-gnu/zeroclaw
# ELF 64-bit LSB pie executable, ARM aarch64, … ld-linux-aarch64.so.1

docker run --rm -v "$PWD":/build -w /build rust:1-bookworm \
  readelf -V dist/bin/aarch64-unknown-linux-gnu/zeroclaw \
  | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1
# expect: GLIBC_2.34

# Prove THIS build's JS is inside the ELF (example hash will differ):
FP=$(grep -oE 'assets/index-[A-Za-z0-9_-]+\.js' web/dist/index.html | head -1)
strings dist/bin/aarch64-unknown-linux-gnu/zeroclaw | grep -F "$FP"
```

`GLIBC_2.34` works on Raspberry Pi OS (glibc 2.36+) and Ubuntu 24.04 (2.39).

---

## Deploy to the device

```bash
# from the build machine — use dist/bin, not an unrelated stale path
scp dist/bin/aarch64-unknown-linux-gnu/zeroclaw user@pi:~/zeroclaw.new

# on the Pi
systemctl stop zeroclaw   # if service-managed; kill alone lets systemd restart the old binary
mkdir -p ~/.cargo/bin
cp ~/zeroclaw.new ~/.cargo/bin/zeroclaw && chmod +x ~/.cargo/bin/zeroclaw
zeroclaw --version
systemctl start zeroclaw
```

Install where `which zeroclaw` resolves (usually `~/.cargo/bin`, **not**
`/usr/local/bin`). Hard-refresh the browser (hashed assets + possible CDN /
browser cache of `index.html` is rare, but SPA shells can stick).

With `embedded-web`, you do **not** need to sync a separate `web/dist` tree to
the device unless you intentionally override `gateway.web_dist_dir` and prefer
filesystem assets (embedded still wins for `/_app/*` when the feature is on —
see `crates/zeroclaw-gateway/src/static_files.rs`).

---

## Differences from the CI pipeline

| | Local (`build-cli-local.sh`) | CI (`build-cli-binaries.md`) |
|---|---|---|
| Trigger | manual | version tag / Actions |
| Base image | `rust:1-bookworm` | `ubuntu-22.04` |
| arm64 glibc floor | `GLIBC_2.34` | `GLIBC_2.34` |
| Web embed | **required** (`embedded-web` + fingerprint assert) | must also pass `embedded-web` (see workflow) |
| Output | `dist/bin/<triple>/zeroclaw` | GitHub Release `.tar.gz` / `.zip` |

---

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| WebUI unchanged after binary deploy | Built **without** `embedded-web`; device still serves old `web_dist_dir` | Use `scripts/dev/build-cli-local.sh` only |
| WebUI still stale despite embed | Stale `web/dist` or gateway not cleaned | Do not use `--skip-web`; let the script wipe + rebuild + `cargo clean -p zeroclaw-gateway` |
| Script: fingerprint assert failed | Embed path broken or wrong binary | Check `--features embedded-web`; confirm `web/dist/index.html` exists before Docker build |
| Container: `working directory … is invalid` | MSYS path rewrite on Git Bash | Use the script (sets `MSYS_NO_PATHCONV`); or export it yourself |
| `missing required command: cargo` under `bash` | WSL bash, not Git Bash | Use `scripts\dev\build-cli-local.cmd`, or let the `.sh` re-exec under Git Bash |
| `build dir not found: …\cursor-sandbox-cache\…\cargo-target` | Host `CARGO_TARGET_DIR` ≠ Docker's `$REPO_ROOT/target` | Script unsets it; `collect-dist` also falls back to repo `target/` |
| `cargo web build` fails | OpenAPI / `openapi-typescript` | Never substitute `cd web && npm run build` |
| `GLIBC_x.y not found` on device | Target glibc older than floor | Lower container base glibc; never raise it for Pi |
| Hand-edited `dist/bin/…` ≠ `target/…` | Forgot `collect-dist` / ran an old copy | Re-run the script; trust the printed sha256 |

---

## Related

- `scripts/dev/build-cli-local.sh` — canonical local builder (this runbook).
- `scripts/dev/build-cli-local.cmd` — Windows entrypoint that always uses Git Bash.
- `scripts/dev/collect-dist.sh` — copies `target/<triple>/release/*` → `dist/bin/<triple>/`.
- `scripts/dev/reclaim-cargo-disk.sh` / `.cmd` — wipe compile/test residue (`target/`, `%TEMP%\zc-*`, …) after builds/tests.
- [`build-cli-binaries.md`](./build-cli-binaries.md) — fork CI release pipeline.
- `docs/book/src/hardware/raspberry-pi-setup.md` — Pi deploy context.
- `docs/book/src/developing/web.md` — `cargo web` surface.
