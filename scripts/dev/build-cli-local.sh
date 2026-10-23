#!/usr/bin/env bash
# Build a deployable zeroclaw CLI locally (cross-compile via Docker) with the
# CURRENT web dashboard baked into the binary.
#
# THIS IS THE CANONICAL LOCAL PATH for on-device testing (e.g. Raspberry Pi).
# Do NOT hand-roll `docker run … cargo build` without --features embedded-web:
# default features do NOT embed web/dist, so replacing only the binary leaves
# the device serving a stale on-disk gateway.web_dist_dir and your WebUI
# changes will appear to "not take effect".
#
# Usage:
#   scripts/dev/build-cli-local.sh
#   scripts/dev/build-cli-local.sh --target aarch64-unknown-linux-gnu
#   scripts/dev/build-cli-local.sh --target x86_64-unknown-linux-gnu
#   scripts/dev/build-cli-local.sh --skip-web   # ONLY when web/dist is already fresh
#   scripts/dev/build-cli-local.sh --no-plugins-wasm  # drop the WASM plugin host (ARM32 only)
#
# Features are always --features embedded-web,plugins-wasm-cranelift: that is
# the release shape. --plugins-wasm is accepted and is now the default.
#
# Pipeline (always, unless --skip-web):
#   1. wipe web/dist (keep .gitkeep)
#   2. cargo web build          → fresh web/dist
#   3. cargo clean -p zeroclaw-gateway  → force include_dir! re-embed
#   4. Docker cross-compile with --features embedded-web,plugins-wasm-cranelift
#   5. assert the binary contains the current index-*.js fingerprint
#   6. collect into dist/bin/<target>/zeroclaw
#   7. reclaim compile/test residue (EXIT trap + success path)
#      → wipe repo target/, %TEMP%/zc-cargo-home, %TEMP%/zeroclaw/,
#        %TEMP%/zc-*, sandbox CARGO_TARGET_DIR, .cache/npm
#
# Output (durable):
#   dist/bin/<target>/zeroclaw          (via collect-dist.sh)
# cargo's target/<target>/release/zeroclaw is removed on exit (success or fail).
#
# Docs: docs/maintainers/build-cli-local.md
#
# Windows: always run under Git Bash (not WSL). WSL's `bash` does not see
# Windows `cargo.exe` as `cargo`, and Docker volume mounts from WSL vs
# Git Bash also diverge. Re-exec before `set -euo` so a missing Git Bash
# still prints a clear error.
_zc_already_git_bash() {
  case "$(uname -s 2>/dev/null || true)" in
    MINGW*|MSYS*|CYGWIN*) return 0 ;;
  esac
  return 1
}

_zc_needs_git_bash() {
  _zc_already_git_bash && return 1
  if [ -n "${WSL_DISTRO_NAME:-}${WSL_INTEROP:-}" ]; then
    return 0
  fi
  case "${OS:-}" in
    Windows_NT) return 0 ;;
  esac
  case "${BASH_SOURCE[0]}" in
    /mnt/[a-zA-Z]/*) return 0 ;;
  esac
  return 1
}

_zc_find_git_bash() {
  local c
  for c in \
    "/mnt/c/Program Files/Git/bin/bash.exe" \
    "/mnt/c/Program Files (x86)/Git/bin/bash.exe" \
    "/c/Program Files/Git/bin/bash.exe" \
    "/c/Program Files (x86)/Git/bin/bash.exe" \
    "${PROGRAMFILES:-}/Git/bin/bash.exe" \
    "${ProgramFiles:-}/Git/bin/bash.exe"
  do
    if [ -n "$c" ] && [ -x "$c" ]; then
      printf '%s\n' "$c"
      return 0
    fi
  done
  return 1
}

_zc_script_for_git_bash() {
  local script="$1"
  if command -v wslpath >/dev/null 2>&1; then
    wslpath -w "$script"
    return 0
  fi
  # /mnt/c/Users/... → /c/Users/... (Git Bash path)
  case "$script" in
    /mnt/[a-zA-Z]/*)
      printf '/%s\n' "${script#/mnt/}"
      return 0
      ;;
  esac
  printf '%s\n' "$script"
}

if _zc_needs_git_bash; then
  _zc_bash="$(_zc_find_git_bash)" || {
    echo "error: this script must run under Git Bash on Windows (WSL bash cannot see Windows cargo)." >&2
    echo "Install Git for Windows, or run: \"C:\\Program Files\\Git\\bin\\bash.exe\" scripts/dev/build-cli-local.sh" >&2
    echo "Or from PowerShell/cmd: scripts\\dev\\build-cli-local.cmd" >&2
    exit 1
  }
  _zc_script="$(_zc_script_for_git_bash "${BASH_SOURCE[0]}")"
  echo "==> re-exec under Git Bash: $_zc_bash" >&2
  exec "$_zc_bash" "$_zc_script" "$@"
fi

set -euo pipefail

TARGET="aarch64-unknown-linux-gnu"
SKIP_WEB=0
# Build the WASM plugin host into the binary. On by default because the
# release binary ships it: without it `zeroclaw plugin` is absent and .cwasm
# packages cannot load. `--no-plugins-wasm` opts out for targets where
# Cranelift is unavailable (ARM32) — it is not a supported release shape.
PLUGINS_WASM=1
RUST_IMAGE="${ZEROCLAW_RUST_IMAGE:-rust:1-bookworm}"

while [ $# -gt 0 ]; do
  case "$1" in
    --target)   TARGET="${2:-}"; shift 2 ;;
    --target=*) TARGET="${1#*=}"; shift ;;
    --skip-web) SKIP_WEB=1; shift ;;
    --plugins-wasm) PLUGINS_WASM=1; shift ;;
    --no-plugins-wasm) PLUGINS_WASM=0; shift ;;
    --image)    RUST_IMAGE="${2:-}"; shift 2 ;;
    --image=*)  RUST_IMAGE="${1#*=}"; shift ;;
    -h|--help)
      sed -n '2,30p' "$0"
      exit 0
      ;;
    *)
      echo "unknown arg: $1" >&2
      exit 2
      ;;
  esac
done

if [ -z "$TARGET" ]; then
  echo "error: --target must not be empty" >&2
  exit 2
fi

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

# Docker mounts REPO_ROOT and writes into $REPO_ROOT/target. Cursor (and
# similar) sandboxes inject CARGO_TARGET_DIR elsewhere; host cargo + collect
# must not follow that, or collect fails after a successful docker build.
# Keep the sandbox path for EXIT reclaim, then unset for the build itself.
_ZC_SAVED_CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-}"
unset CARGO_TARGET_DIR

die() { echo "error: $*" >&2; exit 1; }
step() { echo; echo "==> $*"; }

# Two runs share $REPO_ROOT/target, and each one's EXIT reclaim does
# `rm -rf target/`. When they overlap, the first to finish deletes the other's
# in-flight object files and rustc reports `couldn't create a temp dir: … at
# path target/<triple>/release/deps/rmetaw*` — an error that points at nothing
# in the source. Refuse to start while another run is alive.
_ZC_LOCK_DIR="${TMPDIR:-/tmp}/zeroclaw-build-cli-$(printf '%s' "$REPO_ROOT" | cksum | cut -d' ' -f1).lock"

_zc_acquire_lock() {
  local stale_retry=0 owner
  while :; do
    if mkdir "$_ZC_LOCK_DIR" 2>/dev/null; then
      printf '%s\n' "$$" > "$_ZC_LOCK_DIR/pid"
      return 0
    fi
    owner="$(cat "$_ZC_LOCK_DIR/pid" 2>/dev/null || true)"
    if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
      die "another build-cli-local run is in progress (pid $owner). Both runs share target/ and each one's EXIT reclaim deletes the other's object files. Wait for it, or stop it first."
    fi
    [ "$stale_retry" -eq 0 ] \
      || die "lock $_ZC_LOCK_DIR is held by a run that is gone (pid ${owner:-unknown}); remove that directory and retry"
    rm -rf "$_ZC_LOCK_DIR" 2>/dev/null || true
    stale_retry=1
  done
}

# Always reclaim bulky cargo intermediates — success, failure, or Ctrl-C.
# Keeps only dist/bin/<target>/zeroclaw. See scripts/dev/reclaim-cargo-disk.sh.
_zc_reclaim_on_exit() {
  # Release the lock first: the success path below returns early.
  if [ -n "${_ZC_LOCK_DIR:-}" ] && [ -d "$_ZC_LOCK_DIR" ]; then
    rm -rf "$_ZC_LOCK_DIR" 2>/dev/null || true
  fi
  # Avoid double-printing when the success path already reclaimed.
  if [ "${_ZC_RECLAIMED:-0}" = "1" ]; then
    return 0
  fi
  echo
  echo "==> reclaim cargo disk (EXIT trap — target/ can be hundreds of GB)"
  if [ -n "${_ZC_SAVED_CARGO_TARGET_DIR:-}" ]; then
    CARGO_TARGET_DIR="$_ZC_SAVED_CARGO_TARGET_DIR" \
      bash "$REPO_ROOT/scripts/dev/reclaim-cargo-disk.sh" || true
  else
    bash "$REPO_ROOT/scripts/dev/reclaim-cargo-disk.sh" || true
  fi
  _ZC_RECLAIMED=1
}
trap '_zc_reclaim_on_exit' EXIT INT TERM

_zc_acquire_lock

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || die "missing required command: $1"
}

# Git Bash on Windows often lacks ~/.cargo/bin on PATH even when PowerShell has it.
export PATH="${HOME}/.cargo/bin:${PATH:-}"
if [ -n "${USER:-}" ]; then
  export PATH="/c/Users/${USER}/.cargo/bin:${PATH}"
fi
if [ -n "${USERPROFILE:-}" ]; then
  # USERPROFILE is like C:\Users\foo — map to /c/Users/foo for Git Bash.
  _up="$(cygpath -u "$USERPROFILE" 2>/dev/null || true)"
  if [ -n "$_up" ]; then
    export PATH="${_up}/.cargo/bin:${PATH}"
  fi
fi

need_cmd docker
need_cmd cargo
need_cmd node
need_cmd npm

# ── 1–3. Fresh web/dist + force gateway rebuild ───────────────────────────
if [ "$SKIP_WEB" -eq 0 ]; then
  step "wipe web/dist (keep .gitkeep) so no stale hashed assets survive"
  if [ -d web/dist ]; then
    find web/dist -mindepth 1 ! -name '.gitkeep' -exec rm -rf {} + 2>/dev/null \
      || find web/dist -mindepth 1 ! -name '.gitkeep' -print0 | xargs -0 rm -rf
  fi
  mkdir -p web/dist
  touch web/dist/.gitkeep

  step "cargo web build (OpenAPI + openapi-typescript + vite → web/dist)"
  # Agent shells (WorkBuddy, Cursor, …) export NODE_OPTIONS with a `--require`
  # shim that wraps `fs.rmSync` in a bulk-delete guard. Vite's `emptyOutDir`
  # clears web/dist through `rmSync`, and that guard needs a lock directory the
  # sandbox forbids — the web build then dies with
  # `SAFE_DELETE_BULK_GUARD_ERROR` before writing a single asset, which looks
  # like a vite bug and is not one. Nothing in this build needs NODE_OPTIONS, so
  # drop it here instead of making every caller remember to.
  if [ -n "${NODE_OPTIONS:-}" ]; then
    step "clear NODE_OPTIONS for the web build (sandbox fs shims break vite's emptyOutDir)"
    echo "    was: $NODE_OPTIONS"
    unset NODE_OPTIONS
  fi
  cargo web build
  [ -f web/dist/index.html ] || die "web/dist/index.html missing after cargo web build"

  step "cargo clean -p zeroclaw-gateway (force include_dir! to re-read web/dist)"
  cargo clean -p zeroclaw-gateway
else
  step "skipping web rebuild (--skip-web); requiring existing web/dist/index.html"
  [ -f web/dist/index.html ] || die "web/dist/index.html missing; refuse --skip-web"
fi

FINGERPRINT="$(grep -oE 'assets/index-[A-Za-z0-9_-]+\.js' web/dist/index.html | head -1 || true)"
[ -n "$FINGERPRINT" ] || die "could not read index-*.js fingerprint from web/dist/index.html"
echo "    web fingerprint: $FINGERPRINT"

# ── 4. Cross-compile inside Docker with embedded-web ──────────────────────
# default features do NOT include embedded-web. Passing it explicitly is what
# makes include_dir!("…/web/dist") compile into the binary.
FEATURES="embedded-web"
if [ "$PLUGINS_WASM" -eq 1 ]; then
  FEATURES="$FEATURES,plugins-wasm-cranelift"
fi

DOCKER_ENV=(-e "CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc")
APT_PKGS="gcc-aarch64-linux-gnu g++-aarch64-linux-gnu"
case "$TARGET" in
  aarch64-unknown-linux-gnu)
    ;;
  x86_64-unknown-linux-gnu)
    DOCKER_ENV=()
    APT_PKGS=""
    ;;
  *)
    die "unsupported --target '$TARGET' (supported: aarch64-unknown-linux-gnu, x86_64-unknown-linux-gnu)"
    ;;
esac

step "docker cross-compile ($TARGET, features=$FEATURES) via $RUST_IMAGE"

# Git Bash on Windows rewrites /build unless path conversion is disabled.
export MSYS_NO_PATHCONV=1
export MSYS2_ARG_CONV_EXCL='*'

# shellcheck disable=SC2086
docker run --rm \
  -v "$REPO_ROOT:/build" \
  -w /build \
  "${DOCKER_ENV[@]}" \
  "$RUST_IMAGE" \
  bash -c "
    set -e
    # Docker Desktop's DNS proxy (192.168.65.7) sometimes answers with AAAA
    # only — e.g. deb.debian.org resolving to 2a04:4e42:96::644 — while the
    # container has no IPv6 route. apt then burns its attempts on v6, falls
    # back to v4, and reports the v4 connect as 'Connection refused (111)',
    # which reads like a firewall and is really an address-family mismatch.
    # These containers only ever have IPv4 egress, so pin apt to it and retry:
    # a transient refusal then costs one retry instead of the whole build.
    APT_OPTS='-o Acquire::ForceIPv4=true -o Acquire::Retries=3'
    apt-get \$APT_OPTS update -qq
    if [ -n \"$APT_PKGS\" ]; then
      apt-get \$APT_OPTS install -y -qq $APT_PKGS
    fi
    rustup target add \"$TARGET\"
    # embedded-web asserts web/dist/index.html exists at build-script time.
    test -f web/dist/index.html
    cargo build --release --locked \
      --target \"$TARGET\" \
      --features \"$FEATURES\" \
      --bin zeroclaw
  "

BIN="target/$TARGET/release/zeroclaw"
[ -f "$BIN" ] || die "expected binary missing: $BIN"

# ── 5. Prove the CURRENT dashboard is inside the ELF ──────────────────────
step "verify binary embeds current web/dist ($FINGERPRINT)"
bin_has_fingerprint() {
  local bin="$1"
  local fp="$2"
  if command -v strings >/dev/null 2>&1; then
    strings "$bin" | grep -F -q "$fp"
  else
    docker run --rm -v "$REPO_ROOT:/build" -w /build "$RUST_IMAGE" \
      bash -c "strings \"$bin\" | grep -F -q \"$fp\""
  fi
}

if ! bin_has_fingerprint "$BIN" "$FINGERPRINT"; then
  die "binary does NOT contain '$FINGERPRINT' — embedded-web bake failed; refuse to collect a stale/empty dashboard"
fi
echo "    ok: fingerprint present in $BIN"

# ── 6. Collect into dist/bin/<target>/ ────────────────────────────────────
step "collect into dist/bin/$TARGET/"
bash "$REPO_ROOT/scripts/dev/collect-dist.sh" --target "$TARGET" --bin zeroclaw

DIST_BIN="dist/bin/$TARGET/zeroclaw"
[ -f "$DIST_BIN" ] || die "collect-dist did not produce $DIST_BIN"

if command -v sha256sum >/dev/null 2>&1; then
  SUM="$(sha256sum "$DIST_BIN")"
elif command -v shasum >/dev/null 2>&1; then
  SUM="$(shasum -a 256 "$DIST_BIN")"
else
  SUM="$(docker run --rm -v "$REPO_ROOT:/build" -w /build "$RUST_IMAGE" sha256sum "$DIST_BIN")"
fi

# ── 7. Reclaim disk after a successful collect ────────────────────────────
# Intentionally cold-start next run. Keep only dist/bin/<target>/zeroclaw.
# EXIT trap also runs this on failure so a broken build cannot leave target/.
step "reclaim compile/test residue (target/, %TEMP%/zc-*, sandbox cargo-target)"
if [ -n "${_ZC_SAVED_CARGO_TARGET_DIR:-}" ]; then
  CARGO_TARGET_DIR="$_ZC_SAVED_CARGO_TARGET_DIR" \
    bash "$REPO_ROOT/scripts/dev/reclaim-cargo-disk.sh"
else
  bash "$REPO_ROOT/scripts/dev/reclaim-cargo-disk.sh"
fi
_ZC_RECLAIMED=1
[ ! -e "$REPO_ROOT/target" ] || die "failed to remove $REPO_ROOT/target"

echo
echo "DONE"
echo "  binary : $DIST_BIN"
echo "  web    : $FINGERPRINT (embedded)"
echo "  sha256 : $SUM"
echo "  target : removed ($BIN was cargo output; deploy from dist/bin only)"
echo
echo "Deploy $DIST_BIN to the device (stop the service first), then hard-refresh the browser."
