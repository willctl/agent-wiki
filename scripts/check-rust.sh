#!/usr/bin/env bash
# Formats, lints and tests the Rust workspace for every OS CI builds, from one Linux, WSL or macOS
# shell, so code behind #[cfg(windows)] or #[cfg(target_os = "macos")] is checked before a push.
# CI runs the same lints on each OS, but only after the push lands.
#
#   scripts/check-rust.sh
#
# Needs rustup. It adds the x86_64-pc-windows-gnu and aarch64-apple-darwin targets if missing. The
# Windows lint needs MinGW (apt install gcc-mingw-w64-x86-64). Off macOS, the tray is linted for
# macOS only on a Mac: its macOS dependencies need Apple's SDK to build.
set -euo pipefail
cd "$(dirname "$0")/../rust"
host=$(rustc -vV | sed -n 's/^host: //p')
step() { printf '\n== %s\n' "$*"; }

step "rustfmt"
cargo fmt --all --check
step "clippy ($host)"
cargo clippy --workspace --all-targets -- -D warnings
if [[ "$host" != *windows* ]]; then
  rustup target add x86_64-pc-windows-gnu >/dev/null 2>&1 || true
  step "clippy (x86_64-pc-windows-gnu)"
  cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets -- -D warnings
fi
if [[ "$host" != *apple-darwin ]]; then
  rustup target add aarch64-apple-darwin >/dev/null 2>&1 || true
  step "clippy (aarch64-apple-darwin: core and cli; the tray needs a Mac)"
  cargo clippy --target aarch64-apple-darwin -p aw-core -p agent-wiki --all-targets -- -D warnings
fi
step "tests ($host)"
cargo test --workspace
printf '\nAll Rust checks passed for %s, Windows and macOS.\n' "$host"
