#!/usr/bin/env bash
# Reclaim disk after ZeroClaw cargo builds / tests.
#
# Mandatory wipe list (compile/test residue only — do not touch unrelated Temp):
#   - $REPO_ROOT/target/                 (cargo intermediates; can be 100s of GB)
#   - $TEMP/zc-cargo-home                (agent/sandbox CARGO_HOME; ~1 GB)
#   - $TEMP/zeroclaw/                    (agent temp workspace)
#   - $TEMP/zc-*                         (zc-check.ps1, zc-baseline-check.log,
#                                        zc-docker-*.sh, …)
# Also removes repo .cache/npm (Windows web-build workaround) and a sandbox
# CARGO_TARGET_DIR / …/cargo-target when present (Cursor redirect; can be
# tens of GB). Does NOT run docker system prune.
#
# Residue the host user cannot delete — the cross-compile in
# scripts/dev/build-cli-local.sh runs without `--user`, so it writes target/
# as root — is retried from inside a local container image, where the same
# repo mount is writable. No image is pulled for this: a fallback is only
# possible when one is already local.
#
# Usage:
#   bash scripts/dev/reclaim-cargo-disk.sh
#   scripts\dev\reclaim-cargo-disk.cmd
#
# Safe to run repeatedly. Keeps dist/bin/, web/dist/, and source trees.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

removed_any=0

# Pick an already-local image able to run `rm -rf` on the mounted repo.
_reclaim_rm_image() {
  local image
  for image in "${ZEROCLAW_RUST_IMAGE:-}" zeroclaw-test rust:1-bookworm; do
    [ -n "$image" ] || continue
    if docker image inspect "$image" >/dev/null 2>&1; then
      printf '%s\n' "$image"
      return 0
    fi
  done
  return 1
}

# 0 = nothing left at $1, 1 = still there (or not reachable from a container).
_reclaim_rm_in_container() {
  local path="$1" rel image
  case "$path" in
    "$REPO_ROOT"/*) rel="${path#"$REPO_ROOT"/}" ;;
    *) return 1 ;;
  esac
  command -v docker >/dev/null 2>&1 || return 1
  if ! image="$(_reclaim_rm_image)"; then
    echo "    (still there; no local docker image to remove it with)" >&2
    return 1
  fi
  echo "    (still there after the host-side rm — removing $rel from inside $image)"
  # MSYS_NO_PATHCONV: Git Bash must not rewrite /work/<rel> into a Windows path.
  MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' docker run --rm \
    -v "$REPO_ROOT:/work" -w /work "$image" \
    rm -rf -- "/work/$rel" >/dev/null 2>&1 || true
  [ ! -e "$path" ]
}

wipe_path() {
  local path="$1"
  local label="$2"
  if [ -e "$path" ]; then
    echo "==> remove $label: $path"
    # A failing rm must not abort under `set -e`: the fallback below is the
    # reason this is tolerated here.
    rm -rf "$path" 2>/dev/null || true
    if [ -e "$path" ]; then
      _reclaim_rm_in_container "$path" || true
    fi
    if [ -e "$path" ]; then
      echo "error: failed to remove $path" >&2
      exit 1
    fi
    removed_any=1
  fi
}

# Resolve Windows %TEMP% / Unix $TMPDIR into bash paths.
_temp_roots=()
_add_temp() {
  local raw="$1"
  [ -n "$raw" ] || return 0
  local converted="$raw"
  if command -v cygpath >/dev/null 2>&1; then
    converted="$(cygpath -u "$raw" 2>/dev/null || printf '%s' "$raw")"
  fi
  [ -d "$converted" ] || return 0
  local existing
  for existing in "${_temp_roots[@]+"${_temp_roots[@]}"}"; do
    [ "$existing" = "$converted" ] && return 0
  done
  _temp_roots+=("$converted")
}

_add_temp "${TEMP:-}"
_add_temp "${TMP:-}"
_add_temp "${TMPDIR:-}"
_add_temp "${LOCALAPPDATA:-}/Temp"
_add_temp "/tmp"

# 1) Repo cargo target/ — primary multi-hundred-GB offender.
wipe_path "$REPO_ROOT/target" "repo cargo target/"

# 2–4) Agent/build temps under %TEMP%.
for tmp in "${_temp_roots[@]+"${_temp_roots[@]}"}"; do
  wipe_path "$tmp/zc-cargo-home" "temp zc-cargo-home"
  wipe_path "$tmp/zeroclaw" "temp zeroclaw/"
  # zc-* scripts/logs/dirs (zc-check.ps1, zc-docker-*.sh, …).
  # Nullglob: skip the literal glob when nothing matches.
  shopt -s nullglob
  for path in "$tmp"/zc-*; do
    base="$(basename "$path")"
    # zc-cargo-home already handled; avoid double message.
    [ "$base" = "zc-cargo-home" ] && continue
    wipe_path "$path" "temp $base"
  done
  shopt -u nullglob
done

# Host npm cache used only as a Windows sandbox workaround during web build.
wipe_path "$REPO_ROOT/.cache/npm" "repo .cache/npm"
if [ -d "$REPO_ROOT/.cache" ] && [ -z "$(find "$REPO_ROOT/.cache" -mindepth 1 -print -quit 2>/dev/null)" ]; then
  rmdir "$REPO_ROOT/.cache" 2>/dev/null || true
fi

# Cursor sandbox CARGO_TARGET_DIR (separate from repo target/; can be huge).
if [ -n "${CARGO_TARGET_DIR:-}" ] && [ -e "$CARGO_TARGET_DIR" ]; then
  case "$CARGO_TARGET_DIR" in
    *cursor-sandbox-cache*|*cargo-target*|*/target)
      wipe_path "$CARGO_TARGET_DIR" "sandbox CARGO_TARGET_DIR"
      ;;
  esac
fi

# Named cargo-target dirs under cursor-sandbox-cache only (not the whole cache).
for tmp in "${_temp_roots[@]+"${_temp_roots[@]}"}"; do
  sandbox="$tmp/cursor-sandbox-cache"
  [ -d "$sandbox" ] || continue
  while IFS= read -r -d '' path; do
    wipe_path "$path" "sandbox cargo-target"
  done < <(find "$sandbox" -type d -name 'cargo-target' -print0 2>/dev/null || true)
done

if [ "$removed_any" -eq 0 ]; then
  echo "==> nothing to reclaim (target/ and zc-* temps already gone)"
else
  echo "==> disk reclaim complete (dist/bin kept; Docker images untouched)"
fi
