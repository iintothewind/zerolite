# Build Runbook: CLI Binary, Locally (Docker cross-compile)

Builds the `zeroclaw` CLI on your own machine, by default Linux arm64 for on-device testing (Raspberry Pi, Graviton). It publishes nothing; the CI release path is [`build-cli-binaries.md`](./build-cli-binaries.md).

## Command

```bash
bash scripts/dev/build-cli-local.sh     # Linux / macOS / Git Bash
scripts\dev\build-cli-local.cmd         # Windows PowerShell / cmd
```

| Flag | Effect |
|---|---|
| `--target <triple>` | `aarch64-unknown-linux-gnu` (default) or `x86_64-unknown-linux-gnu` |
| `--image <image>` (or `ZEROCLAW_RUST_IMAGE`) | Build container, default `rust:1-bookworm`. `ghcr.io/iintothewind/zeroclaw-cross-aarch64:bookworm` already carries the cross toolchain, so the apt step is skipped (recipe `dev/cross/Dockerfile`, published by `.github/workflows/cross-image.yml`) |
| `--skip-web` | Reuse the existing `web/dist`. Only when it was rebuilt in this session |
| `--no-plugins-wasm` | Drop `plugins-wasm-cranelift`, for targets where Cranelift is unavailable (ARM32). Not a release shape: the binary has no WASM plugin host and no `zeroclaw plugin` |

Features are always `embedded-web,plugins-wasm-cranelift`, the release shape. Output: `dist/bin/<triple>/zeroclaw`; deploy only this path.

Prerequisites: Docker, and on the host `cargo`, `node` (version in `.nvmrc`) and `npm`. The cross gcc and the Rust target are installed inside the container.

## What the script does

1. Wipes `web/dist` (keeps `.gitkeep`) and runs `cargo web build` (OpenAPI spec, `openapi-typescript`, Vite). It clears `NODE_OPTIONS` first: agent shells inject an `fs.rmSync` guard there that kills Vite's `emptyOutDir` with `SAFE_DELETE_BULK_GUARD_ERROR`.
2. Runs `cargo clean -p zeroclaw-gateway`, so `include_dir!` re-reads `web/dist`.
3. Cross-compiles in Docker: `cargo build --release --locked --target <triple> --features embedded-web,plugins-wasm-cranelift --bin zeroclaw`.
4. Asserts the binary contains the `assets/index-*.js` fingerprint named in `web/dist/index.html`, and refuses to collect otherwise. On failure it prints the fingerprints found in the binary and on disk, and writes `dist/bin/<triple>/fingerprint-report.txt`.
5. Copies the binary to `dist/bin/<triple>/` (`scripts/dev/collect-dist.sh`) and prints its sha256.
6. Reclaims compile residue on EXIT (success, failure or interrupt) through `scripts/dev/reclaim-cargo-disk.sh`: repo `target/`, `%TEMP%\zc-cargo-home`, `%TEMP%\zeroclaw\`, `%TEMP%\zc-*`, a sandbox `CARGO_TARGET_DIR`, repo `.cache/npm`. Every run is therefore a cold build (10+ minutes). It never runs `docker system prune`.

The container builds as root, so `target/` is root-owned. The reclaim step removes it from inside an already-local image (`$ZEROCLAW_RUST_IMAGE`, else `zeroclaw-test`, else `rust:1-bookworm`) and fails loudly when none exists. Two runs cannot overlap: the script takes a lock, because each run's reclaim deletes `target/`.

After an ad-hoc `cargo test` / `cargo check` outside this script, run the same reclaim helper (`AGENTS.md`, disk reclaim).

## Why the WebUI goes stale

The pit this script exists for: Rust changes are live on the device, the dashboard still looks like yesterday.

- **`embedded-web` is not a Cargo default feature.** A plain `cargo build --release --bin zeroclaw` does not embed `web/dist`; the gateway serves whatever is on disk at `gateway.web_dist_dir` on the device, and replacing the binary changes nothing in the UI.
- **`include_dir!` freezes `web/dist` at compile time.** A stale `web/dist`, or a gateway crate not rebuilt after it changed, ships the old hashed assets.

So do not:

- hand-roll `docker … cargo build` without `--features embedded-web`;
- replace `cargo web build` with `cd web && npm run build` (it skips the OpenAPI / `openapi-typescript` step);
- deploy a `target/…/zeroclaw` that never passed the fingerprint assert;
- reuse an old file from `dist/bin/` without re-running the script.

## Verify

The script already fingerprints the dashboard. Manual checks:

```bash
file dist/bin/aarch64-unknown-linux-gnu/zeroclaw
# ELF 64-bit LSB pie executable, ARM aarch64, … ld-linux-aarch64.so.1

docker run --rm -v "$PWD":/build -w /build rust:1-bookworm \
  readelf -V dist/bin/aarch64-unknown-linux-gnu/zeroclaw \
  | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1
# expect: GLIBC_2.34 (Raspberry Pi OS has glibc 2.36+, Ubuntu 24.04 has 2.39)

FP=$(grep -oE 'assets/index-[A-Za-z0-9_-]+\.js' web/dist/index.html | head -1)
strings dist/bin/aarch64-unknown-linux-gnu/zeroclaw | grep -F "$FP"
```

## Deploy to the device

```bash
scp dist/bin/aarch64-unknown-linux-gnu/zeroclaw user@pi:~/zeroclaw.new

# on the device
systemctl stop zeroclaw   # kill alone lets systemd restart the old binary
mkdir -p ~/.cargo/bin
cp ~/zeroclaw.new ~/.cargo/bin/zeroclaw && chmod +x ~/.cargo/bin/zeroclaw
zeroclaw --version
systemctl start zeroclaw
```

Install where `which zeroclaw` resolves (usually `~/.cargo/bin`, **not** `/usr/local/bin`), then hard-refresh the browser. With `embedded-web` there is no separate `web/dist` tree to sync: embedded assets win for `/_app/*` even when `gateway.web_dist_dir` is set (`crates/zeroclaw-gateway/src/static_files.rs`).

## Windows

Run under Git Bash, never WSL: WSL's `bash` does not see Windows `cargo.exe` as `cargo`. `build-cli-local.cmd` launches Git Bash and clears `CARGO_TARGET_DIR`. The `.sh` re-execs under Git Bash when started from WSL, unsets `CARGO_TARGET_DIR` for the host steps so `collect-dist` reads the tree Docker wrote, and sets `MSYS_NO_PATHCONV=1` / `MSYS2_ARG_CONV_EXCL='*'` so `-w /build` is not rewritten to a host path.

## Local vs CI

| | Local (`build-cli-local.sh`) | CI (`build-cli-binaries.md`) |
|---|---|---|
| Trigger | manual | version tag / Actions |
| Base image | `rust:1-bookworm` | `ubuntu-22.04` |
| arm64 glibc floor | `GLIBC_2.34` | `GLIBC_2.34` |
| Web embed | `embedded-web` + fingerprint assert | `embedded-web` |
| Output | `dist/bin/<triple>/zeroclaw` | GitHub Release `.tar.gz` / `.zip` |

## Failure modes

| Symptom | Cause | Fix |
|---|---|---|
| WebUI unchanged after binary deploy | Built **without** `embedded-web`; device still serves the old `web_dist_dir` | Use `scripts/dev/build-cli-local.sh` only |
| WebUI still stale despite embed | Stale `web/dist`, or gateway not cleaned | Do not use `--skip-web`; let the script wipe, rebuild and `cargo clean -p zeroclaw-gateway` |
| Script: fingerprint assert failed | Embed path broken or stale `web/dist` | Check `--features embedded-web`; read the printed embedded list and `dist/bin/<triple>/fingerprint-report.txt` |
| Container: `working directory … is invalid` | MSYS path rewrite on Git Bash | Use the script (sets `MSYS_NO_PATHCONV`), or export it yourself |
| `missing required command: cargo` under `bash` | WSL bash, not Git Bash | Use `scripts\dev\build-cli-local.cmd`, or let the `.sh` re-exec under Git Bash |
| `build dir not found: …\cursor-sandbox-cache\…\cargo-target` | Host `CARGO_TARGET_DIR` differs from Docker's `$REPO_ROOT/target` | The script unsets it; `collect-dist` also falls back to repo `target/` |
| `cargo web build` fails | OpenAPI / `openapi-typescript` step | Never substitute `cd web && npm run build` |
| `GLIBC_x.y not found` on device | Target glibc older than the floor | Lower the container base glibc; never raise it for a Pi |
| Container: apt `Connection refused (111)` fetching `deb.debian.org` | Docker Desktop's network stack refuses the v4 connect, often after an AAAA-only DNS answer with no IPv6 route | The script pins IPv4 and retries; if it still bites, skip apt with `--image ghcr.io/iintothewind/zeroclaw-cross-aarch64:bookworm` |
| Hand-edited `dist/bin/…` differs from `target/…` | Forgot `collect-dist`, or ran an old copy | Re-run the script; trust the printed sha256 |
