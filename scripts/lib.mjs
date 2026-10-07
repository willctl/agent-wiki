// Shared by install.mjs and uninstall.mjs: install locations, safe config
// edits (backup once, merge, preserve EOL), plugin rendering, CLI helpers.

import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { parse as parseToml } from 'smol-toml';
import { appPaths } from '../src/paths.mjs';

export const REPO = path.resolve(fileURLToPath(new URL('..', import.meta.url)));
export const HOME = os.homedir();
export const fwd = (p) => String(p).replace(/\\/g, '/');

// The installer always installs to the standard locations (src/paths.mjs), whatever AGENT_WIKI_*
// overrides the calling shell has; those are for the runtime and the tests.
const installEnv = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^AGENT_WIKI_/.test(k)));
export const AP = appPaths({ env: installEnv });

export const P = {
  configDir: AP.configDir,
  dataDir: AP.dataDir,
  stateDir: AP.stateDir,
  cacheDir: AP.cacheDir,
  legacyHome: AP.legacyHome,
  runtime: AP.runtimeDir,
  config: AP.configFile,
  state: AP.installState,
  migration: AP.migrationJournal,
  paste: AP.pasteFile,
  market: AP.marketDir,
  serviceDir: AP.serviceDir,
  logs: AP.logDir,
  rendered: path.join(AP.marketDir, 'plugins', 'agent-wiki'),
  defaultWiki: path.join(HOME, 'AgentWiki'),
  codexHome: process.env.CODEX_HOME || path.join(HOME, '.codex'),
  personalMarket: path.join(HOME, '.agents', 'plugins', 'marketplace.json'),
  claudeDir: path.join(HOME, '.claude'),
};
P.codexConfig = path.join(P.codexHome, 'config.toml');
P.codexAgents = path.join(P.codexHome, 'AGENTS.md');
P.claudeMd = path.join(P.claudeDir, 'CLAUDE.md');
P.claudeSettings = path.join(P.claudeDir, 'settings.json');

P.serviceExe = path.join(P.serviceDir, 'AgentWikiService.exe');
P.serviceIni = path.join(P.serviceDir, 'AgentWikiService.ini');
P.trayDir = AP.trayDir;
P.trayExe = path.join(P.trayDir, 'AgentWikiTray.exe');
P.trayIni = path.join(P.trayDir, 'AgentWikiTray.ini');
P.trayIcons = path.join(P.trayDir, 'icons');
P.trayTaskXml = path.join(P.trayDir, 'AgentWikiTray.task.xml');
// The Rust programs (docs/rust-plan.md): the server, hook, curator and service in one program in the
// runtime folder (next to ui/, which it serves), and the tray and window app in the tray folder.
const EXE = process.platform === 'win32' ? '.exe' : '';
P.agentExe = path.join(P.runtime, `agent-wiki${EXE}`);
P.rustTrayExe = path.join(P.trayDir, `agent-wiki-tray${EXE}`);
P.rustTrayIni = path.join(P.trayDir, 'agent-wiki-tray.ini');
P.webviewDir = path.join(P.trayDir, 'webview');
P.curatorDir = AP.curatorDir;
P.curatorCodexHome = AP.curatorCodexHome;
P.uiProfile = AP.uiProfile;

export const RUN_KEY = 'HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Run';
export const TRAY_RUN_VALUE = 'AgentWikiTray';
export const TRAY_TASK = 'AgentWikiTray'; // Task Scheduler, root folder (a subfolder outlives its last task)
// Absolute: Git for Windows puts its own (GNU) whoami.exe first on PATH in its shells.
const WHOAMI = path.join(process.env.SystemRoot || 'C:\\Windows', 'System32', 'whoami.exe');

export const SERVICE_NAME = 'AgentWiki';
export const SERVICE_ACCOUNT = `NT SERVICE\\${SERVICE_NAME}`;
export const DEFAULT_PORT = 47821;
export const PLUGIN = 'agent-wiki';
export const CLAUDE_MARKET = 'agent-wiki-local';
export const CLAUDE_PLUGIN_ID = `${PLUGIN}@${CLAUDE_MARKET}`;
export const SERVER = 'agent-wiki';
export const BLOCK_START = '<!-- AGENT-WIKI:START (managed by the agent-wiki installer; replaced on re-install) -->';
export const BLOCK_END = '<!-- AGENT-WIKI:END -->';
export const TOML_COMMENT = '# agent-wiki: run the wiki tools without confirmation (managed by the agent-wiki installer)';

// ---------------------------------------------------------------- output

let quiet = false;
export const setQuiet = (q) => (quiet = q);
export const say = (...a) => !quiet && console.log(...a);
export const step = (n, title) => say(`\n[${n}] ${title}`);
export const info = (msg) => say(`    ${msg}`);
export const warn = (msg) => console.log(`    ! ${msg}`);
export class InstallError extends Error {}

// ---------------------------------------------------------------- fs

export const exists = (p) => fs.existsSync(p);

export async function readText(file) {
  try {
    return await fsp.readFile(file, 'utf8');
  } catch (e) {
    if (e.code === 'ENOENT') return null;
    throw e;
  }
}

const eolOf = (text) => (text && /\r\n/.test(text) ? '\r\n' : '\n');
const withEol = (text, eol) => text.replace(/\r\n?/g, '\n').replace(/\n/g, eol);

async function atomicWrite(file, content) {
  await fsp.mkdir(path.dirname(file), { recursive: true });
  const tmp = `${file}.agent-wiki-${process.pid}.tmp`;
  await fsp.writeFile(tmp, content, 'utf8');
  for (let i = 0; ; i++) {
    try {
      await fsp.rename(tmp, file);
      return;
    } catch (e) {
      if (i >= 20 || !['EPERM', 'EACCES', 'EBUSY'].includes(e.code)) {
        await fsp.rm(tmp, { force: true });
        throw e;
      }
      await new Promise((r) => setTimeout(r, 50 + i * 50));
    }
  }
}

/** Copies an existing file to <file>.bak-agent-wiki once. The first backup is never overwritten. */
export async function backupOnce(file) {
  if (!exists(file)) return null;
  const bak = `${file}.bak-agent-wiki`;
  if (!exists(bak)) {
    await fsp.copyFile(file, bak, fs.constants.COPYFILE_EXCL);
    info(`backed up ${fwd(file)} -> ${path.basename(bak)}`);
  }
  return bak;
}

/**
 * Read-modify-write of a config file. `transform(text|null)` returns the new
 * text (LF) or null for "no change". Existing files are backed up once and
 * keep their line-ending style; new files are LF.
 */
export async function editFile(file, transform) {
  const cur = await readText(file);
  const next = await transform(cur === null ? null : cur.replace(/\r\n/g, '\n'));
  if (next === null || next === undefined) return false;
  const out = withEol(next, cur === null ? '\n' : eolOf(cur));
  if (out === cur) return false;
  if (cur !== null) await backupOnce(file);
  await atomicWrite(file, out);
  return true;
}

export async function writeFileLF(file, text) {
  await atomicWrite(file, text.replace(/\r\n?/g, '\n'));
}

export async function readJson(file) {
  const t = await readText(file);
  if (t === null) return null;
  try {
    return JSON.parse(t.replace(/^﻿/, ''));
  } catch (e) {
    throw new InstallError(`${fwd(file)} is not valid JSON (${e.message}); fix it or restore its backup, then re-run.`);
  }
}

export const toJson = (obj) => `${JSON.stringify(obj, null, 2)}\n`;

export async function removeEmptyDirs(dir, stopAt) {
  let d = dir;
  while (d.length > stopAt.length && d.startsWith(stopAt)) {
    try {
      if ((await fsp.readdir(d)).length) return;
      await fsp.rmdir(d);
    } catch {
      return;
    }
    d = path.dirname(d);
  }
}

// ---------------------------------------------------------------- state

export async function loadState() {
  return (await readJson(P.state).catch(() => null)) || { created: [] };
}
export async function saveState(state) {
  await writeFileLF(P.state, toJson(state));
}

// ---------------------------------------------------------------- managed Markdown block

const BLOCK_RE = /\n*<!-- AGENT-WIKI:START[^\n]*-->[\s\S]*?<!-- AGENT-WIKI:END -->\n*/;

function splitAroundBlock(text) {
  const m = text.match(BLOCK_RE);
  if (!m) return null;
  return [text.slice(0, m.index).replace(/\s*$/, ''), text.slice(m.index + m[0].length).trim()];
}

/** Replaces the managed block in place, or appends it. Everything else is kept. */
export function upsertBlock(text, inner) {
  const block = `${BLOCK_START}\n${inner.trim()}\n${BLOCK_END}`;
  const cur = text ?? '';
  const [before, after] = splitAroundBlock(cur) ?? [cur.replace(/\s*$/, ''), ''];
  return `${[before, block, after].filter((s) => s.trim()).join('\n\n')}\n`;
}

/** Removes the managed block. Returns null when there is none, '' when nothing else remains. */
export function removeBlock(text) {
  const parts = text ? splitAroundBlock(text) : null;
  if (!parts) return null;
  const rest = parts.filter((s) => s.trim());
  return rest.length ? `${rest.join('\n\n')}\n` : '';
}

// ---------------------------------------------------------------- codex config.toml

export const approvalHeader = (pluginId) => `[plugins."${pluginId}".mcp_servers.${SERVER}]`;
const headerRe = (pluginId) =>
  new RegExp(`^\\[\\s*plugins\\s*\\.\\s*"${pluginId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"\\s*\\.\\s*mcp_servers\\s*\\.\\s*"?${SERVER}"?\\s*\\]\\s*(#.*)?$`);

export function tomlGet(text, ...keys) {
  let v = parseToml(text);
  for (const k of keys) v = v?.[k];
  return v;
}

export function validateToml(text, label) {
  try {
    parseToml(text);
  } catch (e) {
    throw new InstallError(`Refusing to write ${label}: the result would not be valid TOML (${e.message.split('\n')[0]}).`);
  }
}

/** Ensures default_tools_approval_mode = "approve" in the plugin's server table. Returns new text or null. */
export function tomlSetApproval(text, pluginId) {
  const cur = text ?? '';
  validateToml(cur, 'config.toml');
  if (tomlGet(cur, 'plugins', pluginId, 'mcp_servers', SERVER, 'default_tools_approval_mode') === 'approve') return null;
  const lines = cur.split('\n');
  const h = lines.findIndex((l) => headerRe(pluginId).test(l.trim()));
  let out;
  if (h >= 0) {
    let end = h + 1;
    while (end < lines.length && !/^\s*\[/.test(lines[end])) end++;
    const k = lines.slice(h + 1, end).findIndex((l) => /^\s*default_tools_approval_mode\s*=/.test(l));
    if (k >= 0) lines[h + 1 + k] = 'default_tools_approval_mode = "approve"';
    else lines.splice(h + 1, 0, 'default_tools_approval_mode = "approve"');
    out = lines.join('\n');
  } else {
    out = `${cur.replace(/\s*$/, '')}\n\n${TOML_COMMENT}\n${approvalHeader(pluginId)}\ndefault_tools_approval_mode = "approve"\n`;
  }
  validateToml(out, 'config.toml');
  if (tomlGet(out, 'plugins', pluginId, 'mcp_servers', SERVER, 'default_tools_approval_mode') !== 'approve') {
    throw new InstallError('config.toml merge did not produce the expected approval setting.');
  }
  return out;
}

/** Removes the approval table (and our comment) for pluginId. Returns new text or null. */
/**
 * Claude Code permission rules that let it call the wiki tools without asking (as config.toml does
 * for ChatGPT/Codex): the plugin's server, and a plain `agent-wiki` server (the Claude desktop config
 * entry, which desktop Code sessions also attach). A rule naming a server allows all of its tools.
 */
export const CLAUDE_ALLOW = [`mcp__plugin_${PLUGIN}_${SERVER}`, `mcp__${SERVER}`];

/** settings.json text with CLAUDE_ALLOW in permissions.allow, or null if already there. Keeps everything else. */
export function claudeAllowSet(text) {
  const s = text ? JSON.parse(text) : {};
  s.permissions ||= {};
  const allow = Array.isArray(s.permissions.allow) ? s.permissions.allow : [];
  const missing = CLAUDE_ALLOW.filter((r) => !allow.includes(r));
  if (!missing.length && Array.isArray(s.permissions.allow)) return null;
  s.permissions.allow = [...allow, ...missing];
  return toJson(s);
}

/** settings.json text without CLAUDE_ALLOW (and without an emptied permissions block), or null if absent. */
export function claudeAllowRemove(text) {
  if (!text) return null;
  const s = JSON.parse(text);
  const allow = s.permissions?.allow;
  if (!Array.isArray(allow) || !allow.some((r) => CLAUDE_ALLOW.includes(r))) return null;
  s.permissions.allow = allow.filter((r) => !CLAUDE_ALLOW.includes(r));
  if (!s.permissions.allow.length) delete s.permissions.allow;
  if (!Object.keys(s.permissions).length) delete s.permissions;
  return toJson(s);
}

export function tomlRemoveApproval(text, pluginId) {
  if (!text) return null;
  const lines = text.split('\n');
  const h = lines.findIndex((l) => headerRe(pluginId).test(l.trim()));
  if (h < 0) return null;
  let start = h;
  if (h > 0 && lines[h - 1].trim() === TOML_COMMENT) start = h - 1;
  let end = h + 1;
  while (end < lines.length && !/^\s*\[/.test(lines[end])) end++;
  // keep trailing comments/blank lines that belong to the next table
  while (end - 1 > h && /^\s*(#.*)?$/.test(lines[end - 1])) end--;
  lines.splice(start, end - start);
  const out = `${lines.join('\n').replace(/\n{3,}/g, '\n\n').replace(/\s*$/, '')}\n`;
  validateToml(out, 'config.toml');
  return out;
}

// ---------------------------------------------------------------- Claude desktop config

/** claude_desktop_config.json locations: MSIX LocalCache (what Store installs read) or %APPDATA%\Claude. */
export function claudeDesktopConfigs() {
  const found = [];
  const local = process.env.LOCALAPPDATA || path.join(HOME, 'AppData', 'Local');
  const pk = path.join(local, 'Packages');
  if (exists(pk)) {
    for (const d of fs.readdirSync(pk)) {
      if (!/^Claude_/i.test(d)) continue;
      const dir = path.join(pk, d, 'LocalCache', 'Roaming', 'Claude');
      if (exists(dir)) found.push(path.join(dir, 'claude_desktop_config.json'));
    }
  }
  const appdata = path.join(process.env.APPDATA || path.join(HOME, 'AppData', 'Roaming'), 'Claude');
  // With an MSIX install the app reads its LocalCache copy, so only fall back to %APPDATA%.
  if (!found.length && exists(appdata)) found.push(path.join(appdata, 'claude_desktop_config.json'));
  return found;
}

// ---------------------------------------------------------------- processes

/** Runs a CLI without a shell. Returns {ok, code, stdout, stderr}. */
export function run(cmd, args, opts = {}) {
  const r = spawnSync(cmd, args, { encoding: 'utf8', windowsHide: true, timeout: 120_000, ...opts });
  return { ok: r.status === 0, code: r.status, stdout: r.stdout || '', stderr: r.stderr || (r.error ? String(r.error.message) : '') };
}

export function which(cmd) {
  const r = run(process.platform === 'win32' ? 'where.exe' : 'which', [cmd]);
  return r.ok ? r.stdout.split(/\r?\n/)[0].trim() : null;
}

/** 8.3 short form of a path with spaces (Windows), or null. */
export function shortPath(p) {
  if (process.platform !== 'win32' || !/\s/.test(p)) return p;
  const r = spawnSync('cmd.exe', ['/d', '/s', '/c', `"for %I in ("${p}") do @echo %~sI"`], {
    encoding: 'utf8',
    windowsHide: true,
    windowsVerbatimArguments: true,
  });
  const out = (r.stdout || '').trim();
  return out && !/\s/.test(out) && exists(out) ? out : null;
}

/** A token that survives bash, cmd.exe and PowerShell: no spaces if at all possible. */
/**
 * A path as one token of a shell command line. Windows: forward slashes, and an 8.3 form instead of
 * quotes when the path has spaces (one unquoted token works alike in cmd, PowerShell and bash). POSIX:
 * single quotes when needed (node under ~/Library/Application Support, say).
 */
export function shellToken(p, platform = process.platform) {
  if (platform !== 'win32') return /^[A-Za-z0-9_./+:@%,=-]+$/.test(p) ? p : `'${String(p).replace(/'/g, "'\\''")}'`;
  const s = shortPath(p);
  return s ? fwd(s) : `"${fwd(p)}"`;
}

export const nodePath = () => process.execPath;
export const hookCommand = (rust = false) => (rust ? `${shellToken(P.agentExe)} hook` : `${shellToken(nodePath())} ${shellToken(path.join(P.runtime, 'session-start.mjs'))}`);

// ---------------------------------------------------------------- the Rust programs

export const RUST_DIR = path.join(REPO, 'rust');
const rustOut = () => path.join(RUST_DIR, 'target', 'release');
export const rustExe = (name) => path.join(rustOut(), `${name}${process.platform === 'win32' ? '.exe' : ''}`);

/**
 * Builds the Rust programs (cargo build --release) and returns the folder with agent-wiki and
 * agent-wiki-tray, or null when they cannot be had (then install-local keeps the Node runtime).
 * Without cargo on PATH, binaries already in rust/target/release (a CI or cross build) are used.
 */
export function rustBuild() {
  const cargo = which('cargo') || [path.join(process.env.LOCALAPPDATA || '', 'cargo', 'bin', 'cargo.exe')].find((p) => p && exists(p));
  if (cargo) {
    const r = run(cargo, ['build', '--release', '--locked'], { cwd: RUST_DIR, timeout: 1_800_000 });
    if (!r.ok) throw new InstallError(`cargo build --release failed:\n${r.stderr.split('\n').slice(-30).join('\n')}`);
    return { dir: rustOut(), built: true };
  }
  if (exists(rustExe('agent-wiki')) && exists(rustExe('agent-wiki-tray'))) return { dir: rustOut(), built: false };
  return null;
}

/**
 * Puts `src` at `dest` unless it is already there. A program that is running cannot be overwritten on
 * Windows, but it can be renamed: the old one is moved aside (and removed once nothing runs it), and
 * whatever runs it notices the new file (the service and the tray's curator restart on it).
 */
export async function replaceFile(src, dest) {
  const next = await fsp.readFile(src);
  const cur = await fsp.readFile(dest).catch(() => null);
  if (cur && cur.equals(next)) return 'unchanged';
  await fsp.mkdir(path.dirname(dest), { recursive: true });
  const fresh = `${dest}.new-${process.pid}`;
  await fsp.writeFile(fresh, next, { mode: 0o755 });
  let how = 'installed';
  try {
    await fsp.rename(fresh, dest);
  } catch (e) {
    if (!['EPERM', 'EBUSY', 'EACCES'].includes(e.code)) throw e;
    await fsp.rename(dest, `${dest}.old-${Date.now()}`);
    await fsp.rename(fresh, dest);
    how = 'replaced (the running one was moved aside)';
  }
  await removeOldCopies(dest);
  return how;
}

/** Removes `<file>.old-*` copies that nothing runs any more. */
export async function removeOldCopies(file) {
  const dir = path.dirname(file);
  const base = path.basename(file);
  for (const f of await fsp.readdir(dir).catch(() => [])) {
    if (f.startsWith(`${base}.old-`)) await fsp.rm(path.join(dir, f), { force: true }).catch(() => {});
  }
}

export const sha12 = (file) => crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex').slice(0, 12);

/** The Rust tray's settings (agent-wiki-tray.ini). */
export function rustTrayIni({ agent, wikiDir, logDir, icons, port, codex, codexHome, taskXml, webviewDir }) {
  const n = (p) => (process.platform === 'win32' ? path.win32.normalize(p) : p);
  return [
    '# Written by the agent-wiki installer (npm run install-local). Read by agent-wiki-tray.',
    `agent=${n(agent)}`,
    `wikiDir=${n(wikiDir)}`,
    `logDir=${n(logDir)}`,
    `icons=${n(icons)}`,
    `webviewDir=${n(webviewDir)}`,
    `port=${port}`,
    ...(codex ? [`codex=${n(codex)}`] : []),
    ...(codexHome ? [`codexHome=${n(codexHome)}`] : []),
    ...(process.platform === 'win32' ? [`service=${SERVICE_NAME}`, `task=${TRAY_TASK}`, `taskXml=${n(taskXml)}`, `runValue=${TRAY_RUN_VALUE}`] : []),
    '',
  ].join('\n');
}

// ---------------------------------------------------------------- Windows service

const FRAMEWORK = path.join(process.env.WINDIR || 'C:\\Windows', 'Microsoft.NET', 'Framework64', 'v4.0.30319');
const SERVICE_SRC = path.join(REPO, 'service', 'AgentWikiService.cs');

export const serviceSourceHash = () => crypto.createHash('sha256').update(fs.readFileSync(SERVICE_SRC)).digest('hex');

/**
 * Compiles service/AgentWikiService.cs with the .NET Framework csc.exe that ships
 * with Windows. Explicit references and /noconfig keep the build independent
 * of the current directory (PowerShell's own folder holds .NET Core facades).
 */
export function compileService(outExe) {
  compileCs({ src: SERVICE_SRC, outExe, target: 'exe', refs: ['System.dll', 'System.ServiceProcess.dll'], label: 'the service wrapper' });
}

const TRAY_SRC = path.join(REPO, 'tray', 'AgentWikiTray.cs');
export const TRAY_REFS = ['System.dll', 'System.Drawing.dll', 'System.Windows.Forms.dll', 'System.ServiceProcess.dll', 'System.Web.Extensions.dll'];
/** What the compiled tray is made of: its source and the icon embedded in the exe (shown by Settings and Task Manager). */
export const traySourceHash = (icon) => {
  const h = crypto.createHash('sha256').update(fs.readFileSync(TRAY_SRC));
  if (icon) h.update(fs.readFileSync(icon));
  return h.digest('hex');
};

/** Compiles tray/AgentWikiTray.cs (a WinForms exe without a console window); `icon` becomes the exe's icon. */
export function compileTray(outExe, icon) {
  compileCs({ src: TRAY_SRC, outExe, target: 'winexe', refs: TRAY_REFS, icon, label: 'the tray app' });
}

/** csc with /noconfig /nostdlib+ and full reference paths, run from the output folder. */
function compileCs({ src, outExe, target, refs, icon, label }) {
  const csc = path.join(FRAMEWORK, 'csc.exe');
  if (!exists(csc)) throw new InstallError(`C# compiler not found at ${csc} (.NET Framework 4.x is part of Windows 10/11).`);
  const ref = (dll) => `/reference:${path.join(FRAMEWORK, dll)}`;
  const r = run(
    csc,
    [
      '/nologo', '/noconfig', '/nostdlib+', `/target:${target}`, '/optimize+', '/codepage:65001', `/out:${outExe}`,
      ...(icon ? [`/win32icon:${icon}`] : []),
      ref('mscorlib.dll'), ...refs.map(ref), src,
    ],
    { cwd: path.dirname(outExe) },
  );
  if (!r.ok) throw new InstallError(`Compiling ${label} failed:\n${r.stdout}${r.stderr}`);
}

/** `sc query` works without elevation. Returns null (not installed) or a state such as RUNNING. */
export function serviceState() {
  if (process.platform !== 'win32') return null;
  const r = run('sc.exe', ['query', SERVICE_NAME]);
  if (!r.ok) return null;
  return r.stdout.match(/STATE\s*:\s*\d+\s+(\w+)/)?.[1] ?? 'UNKNOWN';
}

/**
 * The app package whose AppData virtualization captures this process's writes, or null (Windows).
 * Inside an MSIX app (Claude desktop's Code tab, Store PowerShell, ChatGPT desktop) files created under
 * AppData land in %LOCALAPPDATA%\Packages\<package>\LocalCache instead, invisible to the service, the
 * tray at sign-in and every other app. A probe file shows it directly.
 */
export function appDataVirtualized(env = process.env) {
  if (process.platform !== 'win32' || !env.LOCALAPPDATA) return null;
  const name = `agent-wiki-probe-${process.pid}-${Date.now()}`;
  const probe = path.join(env.LOCALAPPDATA, name);
  fs.writeFileSync(probe, '');
  try {
    const root = path.join(env.LOCALAPPDATA, 'Packages');
    for (const pkg of fs.readdirSync(root)) if (fs.existsSync(path.join(root, pkg, 'LocalCache', 'Local', name))) return pkg;
    return null;
  } finally {
    fs.rmSync(probe, { force: true });
  }
}

/** Grants the service's virtual account `perm` (RX or M) on `target`, inherited below it. Your own folders need no admin. */
export function grantService(target, perm) {
  const r = run('icacls.exe', [target, '/grant', `${SERVICE_ACCOUNT}:(OI)(CI)${perm}`, '/Q']);
  if (!r.ok) throw new InstallError(`icacls ${target} failed:\n${r.stdout}${r.stderr}`);
  return `${SERVICE_ACCOUNT} ${perm === 'M' ? 'Modify' : 'Read'} on ${fwd(target)}`;
}

/** The executable the service is registered to start (sc qc BINARY_PATH_NAME, unquoted), or null. */
export function serviceBinPath() {
  if (process.platform !== 'win32') return null;
  const r = run('sc.exe', ['qc', SERVICE_NAME]);
  const m = r.ok && r.stdout.match(/BINARY_PATH_NAME\s*:\s*(.+)$/m);
  if (!m) return null;
  const v = m[1].trim();
  return v.startsWith('"') ? v.slice(1, v.indexOf('"', 1)) : v.split(/\s+/)[0];
}

/**
 * Runs `cmd args` elevated (one UAC prompt) and waits for it. Windows only. Resolves {ok, code, error}:
 * declining the prompt is {ok: false, error: 'declined'}.
 */
export function runElevated(cmd, args) {
  const q = (s) => `'${String(s).replace(/'/g, "''")}'`;
  // Start-Process joins ArgumentList with spaces, so each argument is quoted for the child's command line.
  const argList = args.map((a) => q(/[\s"]/.test(a) ? `"${String(a).replace(/"/g, '\\"')}"` : a)).join(',');
  const ps = `try { $p = Start-Process -FilePath ${q(cmd)} -ArgumentList ${argList} -Verb RunAs -Wait -PassThru -WindowStyle Hidden; exit $p.ExitCode } catch { Write-Output $_.Exception.Message; exit 1223 }`;
  const r = run('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', ps], { timeout: 600_000 });
  if (r.code === 1223) return { ok: false, code: r.code, error: /cancel/i.test(r.stdout) ? 'the UAC prompt was declined' : r.stdout.trim() || 'not elevated' };
  return { ok: r.ok, code: r.code, error: r.ok ? null : `exit ${r.code}` };
}

/** GET http://127.0.0.1:<port>/health; resolves with the JSON body or null. */
export function health(port, timeoutMs = 2000) {
  return new Promise((resolve) => {
    const req = http.get({ host: '127.0.0.1', port, path: '/health', timeout: timeoutMs }, (res) => {
      let body = '';
      res.setEncoding('utf8');
      res.on('data', (d) => (body += d));
      res.on('end', () => {
        try {
          resolve(JSON.parse(body));
        } catch {
          resolve(null);
        }
      });
    });
    req.on('timeout', () => req.destroy());
    req.on('error', () => resolve(null));
  });
}

export async function waitForHealth(port, predicate = (h) => h?.ok, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  let last = null;
  while (Date.now() < deadline) {
    last = await health(port);
    if (predicate(last)) return last;
    await new Promise((r) => setTimeout(r, 500));
  }
  return last;
}

export function serviceIni({ node, runtime, wikiDir, configDir, dataDir, stateDir, port, logDir }) {
  return [
    '# Written by the agent-wiki installer (npm run install-local). Read by the service (agent-wiki service, or AgentWikiService.exe).',
    `node=${path.win32.normalize(node)}`,
    `script=${path.win32.join(runtime, 'server.mjs')}`,
    `wikiDir=${path.win32.normalize(wikiDir)}`,
    `configDir=${path.win32.normalize(configDir)}`,
    `dataDir=${path.win32.normalize(dataDir)}`,
    `stateDir=${path.win32.normalize(stateDir)}`,
    `port=${port}`,
    `logDir=${path.win32.normalize(logDir)}`,
    '',
  ].join('\n');
}

export function trayIni({ node, runtime, wikiDir, uiProfile, logDir, icons, port, codex, codexHome, taskXml }) {
  return [
    '# Written by the agent-wiki installer (npm run install-local). Read by AgentWikiTray.exe.',
    `node=${path.win32.normalize(node)}`,
    `runtime=${path.win32.normalize(runtime)}`,
    `wikiDir=${path.win32.normalize(wikiDir)}`,
    `uiProfile=${path.win32.normalize(uiProfile)}`,
    `logDir=${path.win32.normalize(logDir)}`,
    `icons=${path.win32.normalize(icons)}`,
    `port=${port}`,
    ...(codex ? [`codex=${path.win32.normalize(codex)}`] : []),
    ...(codexHome ? [`codexHome=${path.win32.normalize(codexHome)}`] : []),
    `service=${SERVICE_NAME}`,
    `task=${TRAY_TASK}`,
    ...(taskXml ? [`taskXml=${path.win32.normalize(taskXml)}`] : []),
    `runValue=${TRAY_RUN_VALUE}`,
    '',
  ].join('\n');
}

// ---------------------------------------------------------------- start the tray at sign-in

const xmlText = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');

/**
 * The Task Scheduler definition that starts the tray when you sign in. A task that runs as you, with
 * your normal rights, on your own logon needs no admin. Normal priority (the default, 7, is below
 * normal and would pass to the curator), no run-time limit (the default stops it after 72 h), one
 * instance, and no battery conditions. `enabled: false` is for tests.
 */
export function trayTaskXml({ exe, sid, enabled = true, delaySec = 10 }) {
  if (!/^S-1-\d+(-\d+)+$/.test(String(sid))) throw new InstallError(`not a SID: ${sid}`);
  const win = path.win32.normalize(exe);
  return [
    '<?xml version="1.0" encoding="UTF-16"?>',
    '<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">',
    '  <RegistrationInfo>',
    '    <Author>agent-wiki installer</Author>',
    '    <Description>Starts the Agent Wiki tray (and its curator) when you sign in. Written by npm run install-local; removed by npm run uninstall-local.</Description>',
    '  </RegistrationInfo>',
    '  <Triggers>',
    '    <LogonTrigger>',
    '      <Enabled>true</Enabled>',
    `      <UserId>${sid}</UserId>`,
    `      <Delay>PT${delaySec}S</Delay>`,
    '    </LogonTrigger>',
    '  </Triggers>',
    '  <Principals>',
    '    <Principal id="Author">',
    `      <UserId>${sid}</UserId>`,
    '      <LogonType>InteractiveToken</LogonType>',
    '      <RunLevel>LeastPrivilege</RunLevel>',
    '    </Principal>',
    '  </Principals>',
    '  <Settings>',
    '    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>',
    '    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>',
    '    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>',
    '    <AllowHardTerminate>true</AllowHardTerminate>',
    '    <StartWhenAvailable>false</StartWhenAvailable>',
    '    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>',
    '    <IdleSettings>',
    '      <StopOnIdleEnd>false</StopOnIdleEnd>',
    '      <RestartOnIdle>false</RestartOnIdle>',
    '    </IdleSettings>',
    '    <AllowStartOnDemand>true</AllowStartOnDemand>',
    `    <Enabled>${enabled}</Enabled>`,
    '    <Hidden>false</Hidden>',
    '    <RunOnlyIfIdle>false</RunOnlyIfIdle>',
    '    <WakeToRun>false</WakeToRun>',
    '    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>',
    '    <Priority>4</Priority>',
    '  </Settings>',
    '  <Actions Context="Author">',
    '    <Exec>',
    `      <Command>"${xmlText(win)}"</Command>`,
    '      <Arguments>--from task</Arguments>',
    `      <WorkingDirectory>${xmlText(path.win32.dirname(win))}</WorkingDirectory>`,
    '    </Exec>',
    '  </Actions>',
    '</Task>',
    '',
  ].join('\n');
}

/** schtasks /xml reads UTF-16 only ("unable to switch the encoding" for UTF-8). */
export const taskXmlBytes = (xml) => Buffer.concat([Buffer.from([0xff, 0xfe]), Buffer.from(xml, 'utf16le')]);

/** Registers (or replaces) a task from an XML file. Runs as you; no admin for a task like trayTaskXml's. */
export function registerTask(name, xmlFile) {
  const r = run('schtasks.exe', ['/create', '/tn', name, '/xml', xmlFile, '/f']);
  if (!r.ok) throw new InstallError(`Task Scheduler refused task "${name}":\n${r.stdout}${r.stderr}`);
}

/** {exists, enabled, command} of a task (command without quotes). */
export function queryTask(name) {
  const r = run('schtasks.exe', ['/query', '/tn', name, '/xml', 'ONE']);
  if (!r.ok) return { exists: false, enabled: false, command: null };
  const settings = r.stdout.match(/<Settings>[\s\S]*?<\/Settings>/)?.[0] ?? '';
  const command = r.stdout.match(/<Command>([^<]*)<\/Command>/)?.[1].replace(/&amp;/g, '&').replace(/^"|"$/g, '') ?? null;
  return { exists: true, enabled: !/<Enabled>\s*false\s*<\/Enabled>/.test(settings), command };
}

export function deleteTask(name) {
  return run('schtasks.exe', ['/delete', '/tn', name, '/f']).ok;
}

/** The data of a registry value (REG_SZ), or null. `name` null reads the key's default value. */
export function regValue(key, name) {
  const r = run('reg.exe', ['query', key, ...(name === null ? ['/ve'] : ['/v', name])]);
  if (!r.ok) return null;
  const rows = r.stdout.split(/\r?\n/).map((l) => l.match(/^\s+(.+?)\s+REG_\w+\s+(.*)$/)).filter(Boolean);
  // The default value's name is localized ("(Default)", "(Standard)", ...): with /ve it is the only row.
  return (name === null ? rows[0] : rows.find((m) => m[1] === name))?.[2] ?? null;
}

const NOTIFY_KEY = 'HKCU\\Control Panel\\NotifyIconSettings';

/** Explorer's per-app tray icon records: [{key, exe, promoted (true/false/null = never chosen)}]. */
export function notifyIconEntries() {
  const r = run('reg.exe', ['query', NOTIFY_KEY, '/s']);
  const out = [];
  let cur = null;
  for (const line of r.stdout.split(/\r?\n/)) {
    if (/^HKEY_/.test(line)) {
      cur = { key: line.trim(), exe: null, promoted: null };
      out.push(cur);
    } else if (cur) {
      const m = line.match(/^\s+(\S+)\s+REG_\w+\s+(.*)$/);
      if (m?.[1] === 'ExecutablePath') cur.exe = m[2].trim();
      if (m?.[1] === 'IsPromoted') cur.promoted = /0x0*1$/i.test(m[2].trim());
    }
  }
  return out.filter((e) => e.exe);
}

export const PIN_TRAY_ICON = 'Settings > Personalization > Taskbar > Other system tray icons > Agent Wiki: On';

/**
 * Where Windows 11 shows the tray icon. New icons go to the hidden overflow (^) and only you can pin
 * them: Explorer keeps the per-icon choice (IsPromoted) in memory and writes it back when it exits, so
 * a value set from outside neither shows while it runs nor survives a restart (seen here: written on
 * 2026-10-01, gone after the next reboot). Report-only. Returns {promoted: true | false | null, text}.
 */

/** Microsoft Edge (App Paths, machine then user), or null. */
export function edgePath() {
  for (const hive of ['HKLM', 'HKCU']) {
    const v = regValue(`${hive}\\SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\App Paths\\msedge.exe`, null);
    const p = v?.replace(/^"|"$/g, '');
    if (p && exists(p)) return p;
  }
  const guess = path.join(process.env['ProgramFiles(x86)'] || 'C:\\Program Files (x86)', 'Microsoft', 'Edge', 'Application', 'msedge.exe');
  return exists(guess) ? guess : null;
}

/**
 * Starts the Agent Wiki window's own Edge profile once, headless, so its first visible start shows
 * no first-run bubbles over the window. Returns a short status.
 */
export function warmUiProfile(dir = P.uiProfile) {
  if (exists(path.join(dir, 'Local State'))) return 'ready';
  const edge = edgePath();
  if (!edge) return 'Microsoft Edge not found: the window opens in your default browser';
  const r = run(edge, [`--user-data-dir=${dir}`, '--headless=new', '--no-first-run', '--no-default-browser-check', '--dump-dom', 'about:blank'], { timeout: 60_000 });
  return exists(path.join(dir, 'Local State')) ? 'created' : `could not prepare (${r.code ?? 'timeout'})`;
}

export function trayIconPlacement(exe) {
  const e = notifyIconEntries().find((x) => path.resolve(x.exe).toLowerCase() === path.resolve(exe).toLowerCase());
  if (!e) return { promoted: null, text: `Explorer has not registered it yet; once it shows, to pin it: ${PIN_TRAY_ICON}` };
  if (e.promoted === true) return { promoted: true, text: 'pinned to the taskbar' };
  if (e.promoted === false) return { promoted: false, text: `in the ^ overflow (your choice); to pin it: ${PIN_TRAY_ICON}` };
  return { promoted: null, text: `in the ^ overflow (where Windows 11 puts new icons); to pin it: ${PIN_TRAY_ICON}` };
}

/** Removes Explorer's records for tray exes that no longer exist under `dir` (test runs). */
export function forgetNotifyIcons(dir) {
  const base = fs.realpathSync.native(dir).toLowerCase(); // long form: os.tmpdir() can be an 8.3 path
  let n = 0;
  for (const e of notifyIconEntries()) {
    if (path.resolve(e.exe).toLowerCase().startsWith(base) && !exists(e.exe) && run('reg.exe', ['delete', e.key, '/f']).ok) n++;
  }
  return n;
}

/** The SID of the account running this process (S-1-5-21-... or, for Entra ID accounts, S-1-12-1-...). */
export function currentUserSid() {
  if (process.platform !== 'win32') return null;
  const r = run(WHOAMI, ['/user', '/fo', 'csv', '/nh']);
  return r.stdout.match(/"(S-1-[\d-]+)"/)?.[1] ?? null;
}

// The tray's "Restart service" needs start/stop rights on this one service for your account.
// SERVICE_START (RP) + SERVICE_STOP (WP) + SERVICE_QUERY_STATUS (LC); nothing that changes,
// reconfigures or deletes the service, or its permissions.
export const TRAY_SERVICE_RIGHTS = 'RPWPLC';
const aceFor = (sid) => `(A;;${TRAY_SERVICE_RIGHTS};;;${sid})`;
const sidAceRe = (sid) => new RegExp(`\\(A;;([A-Z]*);;;${sid.replace(/[-]/g, '\\-')}\\)`, 'g');
// Windows stores rights in its own order (RPWPLC comes back as LCRPWP): compare them as sets of 2-letter codes.
const sameRights = (a, b) => (a.match(/../g) || []).sort().join() === (b.match(/../g) || []).sort().join();

/** Adds (or refreshes) the start/stop ACE for `sid` in a service's SDDL DACL. Returns the new SDDL. */
export function sddlWithStartStop(sddl, sid) {
  if (!/^S-1-[\d-]+$/.test(sid)) throw new InstallError(`not a SID: ${sid}`);
  const s = sddl.trim();
  // The SACL ("S:...") follows the DACL ("D:..."); find it outside the parenthesized ACEs.
  let depth = 0;
  let sacl = s.length;
  for (let i = 0; i < s.length; i++) {
    if (s[i] === '(') depth++;
    else if (s[i] === ')') depth--;
    else if (depth === 0 && s.startsWith('S:', i)) {
      sacl = i;
      break;
    }
  }
  if (!s.includes('D:') || s.indexOf('D:') > sacl) throw new InstallError(`unexpected service security descriptor: ${sddl}`);
  const mine = [...s.slice(0, sacl).matchAll(sidAceRe(sid))];
  if (mine.length === 1 && sameRights(mine[0][1], TRAY_SERVICE_RIGHTS)) return s; // already granted
  return `${s.slice(0, sacl).replace(sidAceRe(sid), '')}${aceFor(sid)}${s.slice(sacl)}`;
}

export function sddlWithoutStartStop(sddl, sid) {
  return sddl.trim().replace(sidAceRe(sid), '');
}

/** True when this process runs elevated (High or System integrity level). */
export function isElevated() {
  const r = run(WHOAMI, ['/groups']);
  return /S-1-16-12288|S-1-16-16384/.test(r.stdout);
}

// ---------------------------------------------------------------- plugin rendering

/** Hash of a directory tree (relative paths + contents), for comparing a rendered plugin with an app's cached copy. */
export async function treeHash(dir) {
  if (!exists(dir)) return null;
  const h = crypto.createHash('sha256');
  for (const rel of await walk(dir)) h.update(`${rel}\0`).update(await fsp.readFile(path.join(dir, rel))).update('\0');
  return h.digest('hex');
}

async function walk(dir, base = dir) {
  const out = [];
  for (const e of await fsp.readdir(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) out.push(...(await walk(p, base)));
    else out.push(fwd(path.relative(base, p)));
  }
  return out.sort();
}

function deepReplace(v, map) {
  if (typeof v === 'string') return Object.entries(map).reduce((s, [k, r]) => s.split(k).join(r), v);
  if (Array.isArray(v)) return v.map((x) => deepReplace(x, map));
  if (v && typeof v === 'object') return Object.fromEntries(Object.entries(v).map(([k, x]) => [k, deepReplace(x, map)]));
  return v;
}

/**
 * Renders plugin/ into `dest` with absolute paths filled in. Writes to a
 * staging dir, verifies the tree is exactly the source tree, then swaps it in,
 * so copying into an existing dir can never nest .claude-plugin/.claude-plugin.
 */
export async function renderPlugin(dest, map, { mcpServer } = {}) {
  const src = path.join(REPO, 'plugin');
  const files = await walk(src);
  const staging = `${dest}.staging-${process.pid}`;
  await fsp.rm(staging, { recursive: true, force: true });
  for (const rel of files) {
    const text = (await fsp.readFile(path.join(src, rel), 'utf8')).replace(/\r\n?/g, '\n');
    let out = rel.endsWith('.json') ? toJson(deepReplace(JSON.parse(text), map)) : deepReplace(text, map);
    // The template is the stdio form; with the Windows service running, clients connect over HTTP instead.
    if (rel === '.mcp.json' && mcpServer) out = toJson({ mcpServers: { [SERVER]: mcpServer } });
    await fsp.mkdir(path.dirname(path.join(staging, rel)), { recursive: true });
    await fsp.writeFile(path.join(staging, rel), out, 'utf8');
  }
  const got = await walk(staging);
  if (JSON.stringify(got) !== JSON.stringify(files)) throw new InstallError(`Rendered plugin tree differs from source: ${got.join(', ')}`);
  for (const rel of got) {
    if (/(^|\/)(\.[^/]+-plugin)\/\2\//.test(rel)) throw new InstallError(`Nested plugin dir in ${rel}`);
    const t = await fsp.readFile(path.join(staging, rel), 'utf8');
    const left = t.match(/__[A-Z_]+__/);
    if (left) throw new InstallError(`Unrendered placeholder ${left[0]} in ${rel}`);
    if (t.includes('\r')) throw new InstallError(`CR in rendered ${rel}`);
    if (rel.endsWith('.json')) JSON.parse(t);
  }
  await fsp.rm(dest, { recursive: true, force: true });
  await fsp.mkdir(path.dirname(dest), { recursive: true });
  await fsp.rename(staging, dest);
  return files;
}
