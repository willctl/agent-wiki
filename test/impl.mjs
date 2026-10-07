// Which implementation the tests drive: the Node runtime (default) or the Rust program
// (AGENT_WIKI_IMPL=rust). The suites are the contract the Rust port must meet (docs/rust-plan.md).
//   AGENT_WIKI_RUST_BIN   the agent-wiki binary (default rust/target/{release,debug}/agent-wiki)

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = fileURLToPath(new URL('..', import.meta.url));
export const IMPL = process.env.AGENT_WIKI_IMPL === 'rust' ? 'rust' : 'node';

export function rustBin() {
  if (process.env.AGENT_WIKI_RUST_BIN) return process.env.AGENT_WIKI_RUST_BIN;
  const exe = process.platform === 'win32' ? 'agent-wiki.exe' : 'agent-wiki';
  for (const p of ['release', 'debug']) {
    const f = path.join(REPO, 'rust', 'target', p, exe);
    if (fs.existsSync(f)) return f;
  }
  throw new Error('AGENT_WIKI_IMPL=rust: build the Rust program first (cargo build --release in rust/), or set AGENT_WIKI_RUST_BIN');
}

// The Rust server finds the window's files next to itself; in tests they are in dist/runtime/ui.
if (IMPL === 'rust') process.env.AGENT_WIKI_UI_DIR ||= path.join(REPO, 'dist', 'runtime', 'ui');

/** [command, args] that start the server from `runtimeDir` with server.mjs's flags. */
export function srv(runtimeDir, ...args) {
  return IMPL === 'rust' ? [rustBin(), ['serve', ...args]] : [process.execPath, [path.join(runtimeDir, 'server.mjs'), ...args]];
}

/** {command, args} for StdioClientTransport. */
export function srvT(runtimeDir, ...args) {
  const [command, a] = srv(runtimeDir, ...args);
  return { command, args: a };
}

/** [command, args] that run the curator from `runtimeDir` with curator.mjs's flags. */
export function curatorCmd(runtimeDir, ...args) {
  return IMPL === 'rust' ? [rustBin(), ['curator', ...args]] : [process.execPath, [path.join(runtimeDir, 'curator.mjs'), ...args]];
}

/** [command, args] that run the SessionStart hook. */
export function hookCmd(runtimeDir) {
  return IMPL === 'rust' ? [rustBin(), ['hook']] : [process.execPath, [path.join(runtimeDir, 'session-start.mjs')]];
}
