# syntax=docker/dockerfile:1.7
#
# Build host for
#   scripts/dev/build-cli-local.sh --target aarch64-unknown-linux-gnu
#
# The script can install the cross toolchain itself, and still does when the
# image lacks it. But that apt step is the flakiest part of a local build:
# Docker Desktop's network stack intermittently refuses deb.debian.org for
# minutes while the host fetches the same URL in 200ms, and every refusal costs
# a whole build attempt plus the EXIT reclaim. Baking the toolchain into an
# image takes that step off the critical path — the script probes for
# `aarch64-linux-gnu-gcc` and skips apt when it is already there.
#
# Published by .github/workflows/cross-image.yml to
#   ghcr.io/iintothewind/zeroclaw-cross-aarch64:bookworm
# Pull it once, then:
#   scripts/dev/build-cli-local.sh --image ghcr.io/iintothewind/zeroclaw-cross-aarch64:bookworm
# (or set ZEROCLAW_RUST_IMAGE once in your shell).
#
# This image is amd64. It is a *build host* that emits aarch64 binaries — not
# an aarch64 image. An arm64 variant would only help someone cross-compiling
# from an arm64 machine to aarch64, i.e. a native build that needs no
# cross toolchain at all.

# Pinned rather than `rust:1-bookworm`: a moving base makes a "reproducible"
# image tag meaningless, and the script builds with --locked against a
# Cargo.lock whose MSRV moves with the workspace.
FROM rust:1.99.1-bookworm

ENV DEBIAN_FRONTEND=noninteractive

# gcc/g++ provide the linker the script names in
# CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER; binutils provides the
# objcopy/strip the release profile reaches for. pkg-config + libssl-dev are
# needed because the workspace links openssl. cmake/clang/libclang-dev/lld are
# for build scripts that prefer clang when it is present.
RUN apt-get update -qq \
    && apt-get install -y -qq --no-install-recommends \
        gcc-aarch64-linux-gnu \
        g++-aarch64-linux-gnu \
        binutils-aarch64-linux-gnu \
        pkg-config \
        libssl-dev \
        cmake \
        clang \
        libclang-dev \
        lld \
    && rm -rf /var/lib/apt/lists/*

# Pre-install the target's std so the `rustup target add` the script runs
# inside the container is a no-op instead of a network fetch. With the
# component present it prints "up to date" and returns in well under a second;
# without it, a build needs rust-lang.org to be reachable.
RUN rustup target add aarch64-unknown-linux-gnu

# Links the package to this repository on GHCR, which is what lets the repo's
# GITHUB_TOKEN publish it and keeps the package's permissions tied to the repo.
LABEL org.opencontainers.image.source="https://github.com/iintothewind/zerolite" \
      org.opencontainers.image.description="Cross-compile host for zeroclaw local aarch64-unknown-linux-gnu builds" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
