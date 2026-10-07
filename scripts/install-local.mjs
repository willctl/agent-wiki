// npm run install-local [-- <agent-wiki install options>]   (or uninstall-local)
//
// Builds the payload (scripts/package.mjs: the window, the icons, and the Rust programs with cargo when
// it is there) and runs `agent-wiki install` from it: the installer is the Rust program, and Node is
// needed only for this build step. Options pass through: --wiki-dir <path>, --port N, --no-approve.
//
// A missing Rust build is an error. The retired Node runtime is not a supported install target.

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { packagePayload } from './package.mjs';

const REPO = path.resolve(fileURLToPath(new URL('..', import.meta.url)));
const EXE = process.platform === 'win32' ? '.exe' : '';
const uninstall = process.argv.includes('--uninstall');
const args = process.argv.slice(2).filter((a) => a !== '--uninstall');

if (args.includes('--node')) {
  console.error('The Node runtime is retired. Build the Rust programs or install a Rust release payload.');
  process.exit(1);
}
let dir;
try {
  dir = packagePayload().dir;
} catch (e) {
  console.error(`\nBuild failed: ${e.message}`);
  process.exit(1);
}
const agent = path.join(dir, `agent-wiki${EXE}`);
if (!fs.existsSync(agent)) {
  console.error('The Rust installer is missing from the payload. Build it before installing.');
  process.exit(1);
}
const r = spawnSync(agent, [uninstall ? 'uninstall' : 'install', ...args], { stdio: 'inherit' });
process.exit(r.status ?? 1);
