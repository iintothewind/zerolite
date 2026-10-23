# Toolchain image for `cargo test` / `cargo fmt` / `cargo clippy` on this repo.
#
# `rust:1-bookworm` ships rustc and cargo but no rustfmt or clippy component, so
# `cargo fmt --check` fails outright ("'rustfmt' is not installed for the
# toolchain"). Build this once; every test command then runs in it:
#
#   docker build -f scripts/dev/test-image.Dockerfile -t zeroclaw-test .
#
#   MSYS_NO_PATHCONV=1 docker run --rm \
#     -v "$PWD:/work" -v "$HOME/.cargo:/cargo-host" \
#     -e CARGO_HOME=/cargo-host -e CARGO_TARGET_DIR=/work/target-docker \
#     -w /work zeroclaw-test \
#     cargo test -p zeroclaw-runtime --lib -- --test-threads=1
#
# Keep this file to toolchain components only. System libraries a crate needs
# (gdk-3.0 for `zerocode` / `zeroclaw-desktop`) are not added here: those two
# packages stay excluded from workspace-wide runs.
FROM rust:1-bookworm

RUN rustup component add rustfmt clippy \
    && cargo fmt --version \
    && cargo clippy --version
