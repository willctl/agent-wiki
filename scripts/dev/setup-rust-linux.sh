#!/usr/bin/env bash
# Sets up a Linux (or WSL) machine to build Agent Wiki's Rust code: C toolchain, the GTK/WebKitGTK and
# AppIndicator headers the tray and window need, and Rust in /opt (RUSTUP_HOME, CARGO_HOME), not ~/.cargo.
# Run as root:  sudo bash scripts/dev/setup-rust-linux.sh
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq build-essential pkg-config curl ca-certificates \
  libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev >/dev/null
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
if [ ! -x /opt/cargo/bin/cargo ]; then
  curl -fsSL https://sh.rustup.rs | sh -s -- -y --no-modify-path --default-toolchain stable --profile minimal --component clippy,rustfmt
else
  /opt/cargo/bin/rustup update stable
fi
cat >/etc/profile.d/rust.sh <<'EOF'
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
case ":$PATH:" in *:/opt/cargo/bin:*) ;; *) export PATH="/opt/cargo/bin:$PATH" ;; esac
EOF
for b in cargo rustc rustup; do ln -sf "/opt/cargo/bin/$b" "/usr/local/bin/$b"; done
rustc --version
cargo --version
