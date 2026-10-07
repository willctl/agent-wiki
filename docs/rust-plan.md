# Rust runtime

The shipping programs are agent-wiki and agent-wiki-tray. The core crate owns wiki storage, search, the curator, held changes and Forget. The CLI owns transports, installation and the service. The tray owns native window and platform integration.

## Migration status

The Rust core, server, curator, Ask pipeline, service, tray and installer are implemented. The earlier Node runtime and C# service and tray remain in this source tree for compatibility testing. Their retirement is a separate change. Do not add new features to them.

## Build and verify

Build the window with npm run build. Build the Rust workspace with cargo build --release --locked from rust/. Before pushing Rust changes, run scripts/check-rust.sh in Linux, WSL or macOS. It checks formatting, lint and unit tests, including cross-target lint where supported.

The JavaScript behavior suites run against Rust when AGENT_WIKI_IMPL=rust. See AGENTS.md for the full commands and the isolated verification skill. CI runs native Windows, Linux and macOS jobs.

Use scripts/bench.mjs to measure the two implementations on synthetic data. Keep raw machine-specific output outside tracked files. Report hardware, sample counts and measurement conditions only when those details are intended for publication.
