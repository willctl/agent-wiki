// Assembles what a release ships into one folder (default dist/package): agent-wiki and
// agent-wiki-tray, the window's files (ui/) and the tray icons (icons/). `agent-wiki install` in that
// folder installs from it. Node is needed here only to build the window and the icons.
//
//   node scripts/package.mjs [--out <dir>] [--bin <dir with the Rust programs>] [--no-cargo]
//
// The Rust programs come from `cargo build --release` (when cargo is on PATH or in %LOCALAPPDATA%\cargo,
// unless --no-cargo) or, without cargo, from rust/target/release as they are (a CI or cross build).

import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { buildIcons } from './build-icons.mjs';

const REPO = path.resolve(fileURLToPath(new URL('..', import.meta.url)));
const EXE = process.platform === 'win32' ? '.exe' : '';

function findCargo() {
  const r = spawnSync(process.platform === 'win32' ? 'where.exe' : 'which', ['cargo'], { encoding: 'utf8' });
  if (r.status === 0) return r.stdout.split(/\r?\n/)[0].trim();
  const local = path.join(process.env.LOCALAPPDATA || '', 'cargo', 'bin', `cargo${EXE}`);
  return process.env.LOCALAPPDATA && fs.existsSync(local) ? local : null;
}

/** Builds and assembles the payload. Returns {dir, built} or throws. */
export function packagePayload({ out = path.join(REPO, 'dist', 'package'), bin = null, cargo = true, log = console.log } = {}) {
  const build = spawnSync(process.execPath, [path.join(REPO, 'scripts', 'build.mjs')], { cwd: REPO, encoding: 'utf8' });
  if (build.status !== 0) throw new Error(`build failed:\n${build.stdout}${build.stderr}`);
  buildIcons({ preview: false });
  let built = false;
  const c = !bin && cargo ? findCargo() : null;
  if (c) {
    log(`cargo build --release (${c})`);
    const r = spawnSync(c, ['build', '--release', '--locked'], { cwd: path.join(REPO, 'rust'), stdio: 'inherit' });
    if (r.status !== 0) throw new Error('cargo build --release failed');
    built = true;
  }
  const from = bin || path.join(REPO, 'rust', 'target', 'release');
  const programs = [`agent-wiki${EXE}`, `agent-wiki-tray${EXE}`];
  for (const p of programs) if (!fs.existsSync(path.join(from, p))) throw new Error(`${p} not found in ${from}: build it (cargo build --release in rust/) or pass --bin`);
  fs.rmSync(out, { recursive: true, force: true });
  fs.mkdirSync(out, { recursive: true });
  // A GNU cross build loads WebView2 from a DLL beside the tray (the MSVC build links it in).
  for (const f of [...programs, 'WebView2Loader.dll']) if (fs.existsSync(path.join(from, f))) fs.copyFileSync(path.join(from, f), path.join(out, f));
  fs.cpSync(path.join(REPO, 'dist', 'runtime', 'ui'), path.join(out, 'ui'), { recursive: true });
  fs.mkdirSync(path.join(out, 'icons'));
  for (const f of fs.readdirSync(path.join(REPO, 'dist', 'icons'))) if (/^agent-wiki-[a-z]+(-256)?\.(ico|png|svg)$/.test(f)) fs.copyFileSync(path.join(REPO, 'dist', 'icons', f), path.join(out, 'icons', f));
  return { dir: out, built, from };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const args = process.argv.slice(2);
  const opt = (n) => {
    const i = args.indexOf(n);
    return i >= 0 ? args[i + 1] : undefined;
  };
  try {
    const r = packagePayload({ out: opt('--out') ? path.resolve(opt('--out')) : undefined, bin: opt('--bin') || null, cargo: !args.includes('--no-cargo') });
    console.log(`package -> ${r.dir} (programs ${r.built ? 'built with cargo' : `from ${r.from}`})`);
  } catch (e) {
    console.error(`package failed: ${e.message}`);
    process.exit(1);
  }
}
