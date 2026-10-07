// npm run install-local [-- --wiki-dir <path>] [--no-approve] [--skip-tests] [--node]
//
// Installs the Rust programs (agent-wiki, agent-wiki-tray; built with cargo, or taken from
// rust/target/release) when it can; --node, or no Rust build, keeps the Node runtime and the C# tray.
//
// Idempotent: re-running upgrades in place and never touches wiki content.
// Every existing config file is backed up once as <file>.bak-agent-wiki and
// merged, never replaced.

import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { StreamableHTTPClientTransport } from '@modelcontextprotocol/sdk/client/streamableHttp.js';
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { buildIcons } from './build-icons.mjs';
import { describeProcesses, migrate, migrationItems, packagedCopies, planMigration, processesUnder } from './migrate.mjs';
import {
  CLAUDE_MARKET,
  CLAUDE_PLUGIN_ID,
  DEFAULT_PORT,
  HOME,
  InstallError,
  P,
  PLUGIN,
  REPO,
  RUN_KEY,
  SERVER,
  TRAY_RUN_VALUE,
  TRAY_TASK,
  CLAUDE_ALLOW,
  claudeAllowRemove,
  claudeAllowSet,
  PIN_TRAY_ICON,
  SERVICE_NAME,
  backupOnce,
  claudeDesktopConfigs,
  compileService,
  compileTray,
  currentUserSid,
  queryTask,
  regValue,
  registerTask,
  removeOldCopies,
  replaceFile,
  rustBuild,
  rustExe,
  rustTrayIni,
  sha12,
  taskXmlBytes,
  trayIconPlacement,
  warmUiProfile,
  traySourceHash,
  trayIni,
  trayTaskXml,
  serviceIni,
  serviceBinPath,
  appDataVirtualized,
  grantService,
  serviceSourceHash,
  serviceState,
  runElevated,
  treeHash,
  waitForHealth,
  editFile,
  exists,
  fwd,
  hookCommand,
  info,
  loadState,
  nodePath,
  readJson,
  renderPlugin,
  run,
  saveState,
  step,
  toJson,
  tomlGet,
  tomlRemoveApproval,
  tomlSetApproval,
  upsertBlock,
  warn,
  which,
  writeFileLF,
} from './lib.mjs';

const args = process.argv.slice(2);
const flag = (n) => args.includes(n);
const opt = (n) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : undefined;
};

const pkg = JSON.parse(fs.readFileSync(path.join(REPO, 'package.json'), 'utf8'));
const VERSION = pkg.version;
const POINTER = fs.readFileSync(path.join(REPO, 'protocol', 'POINTER.md'), 'utf8').replace(/\r\n?/g, '\n');
const PROTOCOL = fs.readFileSync(path.join(REPO, 'protocol', 'PROTOCOL.md'), 'utf8').replace(/\r\n?/g, '\n');
const cleanEnv = () => Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^AGENT_WIKI_/.test(k)));
const results = [];
const note = (area, status, detail) => results.push({ area, status, detail });

function mustRun(label, cmd, cmdArgs, opts) {
  const r = run(cmd, cmdArgs, opts);
  if (!r.ok) throw new InstallError(`${label} failed (exit ${r.code}):\n${r.stdout}\n${r.stderr}`.trim());
  return r;
}

function jsonFrom(r) {
  const t = r.stdout.trim();
  const i = t.search(/[[{]/);
  return i >= 0 ? JSON.parse(t.slice(i)) : null;
}

/**
 * Stops what holds files in the old ~/.agent-wiki: the tray (and with it the curator and Ask), the
 * curator's Codex app-server daemon, the service, and the Agent Wiki window. Returns whether it
 * stopped the service, so step 2b starts it again.
 */
async function releaseLegacy(folders) {
  if (process.platform !== 'win32') return false;
  let stopped = false;
  for (const legacy of folders) stopped = (await releaseFolder(legacy)) || stopped;
  return stopped;
}

async function releaseFolder(legacy) {
  const tray = path.join(legacy, 'tray', 'AgentWikiTray.exe');
  if (exists(tray)) {
    const q = run(tray, ['--quit'], { timeout: 40_000 });
    info(q.ok ? 'stopped the tray (and the curator)' : `tray --quit: exit ${q.code}`);
  }
  const oldCfg = (await readJson(path.join(legacy, 'config.json')).catch(() => null)) || (await readJson(P.config).catch(() => null));
  const codexPath = oldCfg?.curator?.codexPath || which('codex');
  const codexHome = path.join(legacy, 'curator', 'codex-home');
  if (codexPath && exists(codexHome)) {
    const d = run(codexPath, ['app-server', 'daemon', 'stop'], { env: { ...process.env, CODEX_HOME: codexHome }, timeout: 30_000 });
    info(`the curator's Codex app-server daemon: ${d.ok ? 'stopped' : (d.stdout + d.stderr).trim().split('\n').pop() || `exit ${d.code}`}`);
  }
  let stopped = false;
  const svcBin = (serviceBinPath() || '').toLowerCase();
  if (serviceState() === 'RUNNING' && (svcBin.startsWith(path.resolve(legacy).toLowerCase()) || svcBin.startsWith(path.resolve(P.dataDir).toLowerCase()))) {
    run('sc.exe', ['stop', SERVICE_NAME]);
    for (let i = 0; i < 40 && serviceState() !== 'STOPPED'; i++) await new Promise((r) => setTimeout(r, 500));
    stopped = serviceState() === 'STOPPED';
    info(stopped ? `stopped the service ${SERVICE_NAME}` : `the service ${SERVICE_NAME} did not stop`);
  }
  // The Agent Wiki window (Edge with the window's own profile): asked to close, then ended.
  for (const base of [legacy, P.dataDir]) {
    const profileDir = path.join(base, 'ui-profile');
    const browsers = processesUnder(profileDir).filter((p) => /^msedge\.exe$/i.test(p.name) && !/--type=/.test(p.cmd || ''));
    for (const p of browsers) run('taskkill.exe', ['/PID', String(p.pid)]);
    for (let i = 0; i < 20 && browsers.length && processesUnder(profileDir).length; i++) await new Promise((r) => setTimeout(r, 500));
    for (const p of processesUnder(profileDir)) run('taskkill.exe', ['/PID', String(p.pid), '/T', '/F']);
    if (browsers.length) info('closed the Agent Wiki window');
  }
  // Left over from the tray or the curator's own Codex home: ours, so ended. Anything else is only reported.
  for (const p of [...processesUnder(legacy), ...processesUnder(P.curatorDir)]) {
    const exe = (p.exe || '').toLowerCase();
    const ours = exe.startsWith(path.resolve(legacy).toLowerCase()) || exe.startsWith(path.resolve(P.curatorDir).toLowerCase()) || /curator\.mjs/.test(p.cmd || '');
    if (!ours) continue;
    run('taskkill.exe', ['/PID', String(p.pid), '/T', '/F']);
    info(`ended pid ${p.pid} ${p.name} (it ran from the old folder)`);
  }
  return stopped;
}

async function main() {
  const state = await loadState();
  state.created ||= [];
  const markCreated = (f) => {
    if (!state.created.includes(fwd(f))) state.created.push(fwd(f));
  };
  console.log(`Agent Wiki installer v${VERSION}`);
  const pkg = appDataVirtualized();
  if (pkg) {
    throw new InstallError(
      `This terminal runs inside the app package ${pkg}. Windows redirects what it writes to AppData into that app's\n` +
        'private storage, where the service, the tray at sign-in and the other AI apps cannot see it. Nothing was changed.\n' +
        'Run it from a normal terminal instead: press Win+R, type cmd, press Enter, then:\n' +
        `  cd /d "${REPO}" && npm run install-local`,
    );
  }

  // 1 -------------------------------------------------------------------------
  step(1, 'Build and test');
  mustRun('build', nodePath(), [path.join(REPO, 'scripts', 'build.mjs')], { cwd: REPO });
  if (flag('--skip-tests')) warn('tests skipped (--skip-tests)');
  else {
    const t = run(nodePath(), ['--test', 'test/e2e.mjs', 'test/installer.mjs', 'test/curator.mjs', 'test/tray.mjs', 'test/ui.mjs', 'test/ask.mjs', 'test/migrate.mjs', 'test/portable.mjs', 'test/memory-eval.mjs'], { cwd: REPO, timeout: 600_000 });
    const pass = t.stdout.match(/ℹ pass (\d+)/)?.[1];
    const fail = t.stdout.match(/ℹ fail (\d+)/)?.[1];
    if (!t.ok || fail !== '0') throw new InstallError(`Tests failed; nothing was installed.\n${t.stdout}\n${t.stderr}`);
    info(`tests: ${pass} passed, ${fail} failed`);
  }

  const rustBuilt = flag('--node') ? null : rustBuild();
  const rust = Boolean(rustBuilt);
  if (rust) info(`Rust programs: ${rustBuilt.built ? 'built with cargo' : 'prebuilt'} in ${fwd(rustBuilt.dir)}`);
  else if (!flag('--node')) warn('no Rust build (cargo not found, none in rust/target/release): installing the Node runtime');

  // 1b ------------------------------------------------------------------------
  step('1b', `Standard locations (moving off ${fwd(P.legacyHome)})`);
  const items = migrationItems(P);
  const captured = packagedCopies(P);
  let migrated = false;
  let stoppedService = false;
  if (captured.length) {
    // An earlier install-local ran inside an app package (see appDataVirtualized): its files are in that
    // package's private storage. Move them to the real folders, which only works from outside a package.
    stoppedService = await releaseLegacy(captured.map((c) => c.legacy));
    for (const c of captured) {
      info(`recovering ${fwd(c.legacy)} (captured by the app package ${c.pkg})`);
      const res = await migrate({ legacy: c.legacy, items: c.items, journal: c.journal, log: info });
      for (const x of [...res.conflicts, ...res.unknown]) warn(`left in ${fwd(c.legacy)}: ${x}`);
    }
    migrated = true;
    const moved = await loadState();
    for (const [k, v] of Object.entries(moved)) state[k] = k === 'created' ? [...new Set([...(v || []), ...state.created])] : v;
  }
  const plan = await planMigration({ legacy: P.legacyHome, items });
  if (!plan.needed) info(`settings ${fwd(P.configDir)}; program files ${fwd(P.dataDir)}; logs ${fwd(P.logs)}`);
  else {
    for (const it of plan.items) info(`${it.action}: ${it.name} -> ${fwd(it.to)}`);
    for (const n of plan.unknown) warn(`${n} in ${fwd(P.legacyHome)} is not Agent Wiki's: it stays, and so does the folder`);
    stoppedService = (await releaseLegacy([P.legacyHome])) || stoppedService;
    const res = await migrate({
      legacy: P.legacyHome,
      items,
      journal: P.migration,
      log: info,
      onBusy: (p) => {
        const holders = processesUnder(p);
        return holders.length
          ? `It is in use by:\n${describeProcesses(holders)}\nClose those, then run install-local again: it resumes where it stopped.`
          : 'Close anything that has it open, then run install-local again: it resumes where it stopped.';
      },
      removeLegacy: false, // after the self-test (step 10)
    });
    for (const c of res.conflicts) warn(c);
    migrated = true;
    // install-state.json just moved: what earlier installs created (for uninstall), your SID, the tray.
    const moved = await loadState();
    for (const [k, v] of Object.entries(moved)) state[k] = k === 'created' ? [...new Set([...(v || []), ...state.created])] : v;
    note(
      'Standard locations',
      res.conflicts.length ? 'check' : 'ok',
      `moved to ${fwd(P.configDir)} and ${fwd(P.dataDir)}${res.conflicts.length ? `; ${res.conflicts.length} item(s) left in ${fwd(P.legacyHome)}` : ''}`,
    );
  }

  // 2 -------------------------------------------------------------------------
  step(2, 'Runtime, config.json and wiki');
  const dist = path.join(REPO, 'dist', 'runtime');
  // The bundles at the top, the tray window's web app in ui/. Unchanged files are not rewritten, so the
  // service (which restarts when server.mjs changes) does not restart for nothing.
  // `keep`: files of this folder that are not in dist/ but stay (the Rust program and copies of it
  // moved aside while it ran).
  const syncDir = async (from, to, keep = () => false) => {
    await fsp.mkdir(to, { recursive: true });
    const entries = await fsp.readdir(from, { withFileTypes: true });
    for (const e of entries) {
      if (e.isDirectory()) {
        await syncDir(path.join(from, e.name), path.join(to, e.name));
        continue;
      }
      const next = await fsp.readFile(path.join(from, e.name));
      const cur = await fsp.readFile(path.join(to, e.name)).catch(() => null);
      if (cur && cur.equals(next)) continue;
      const tmp = path.join(to, `${e.name}.new-${process.pid}`);
      await fsp.writeFile(tmp, next);
      await fsp.rename(tmp, path.join(to, e.name));
    }
    const names = entries.map((e) => e.name);
    for (const f of await fsp.readdir(to)) if (!names.includes(f) && !keep(f)) await fsp.rm(path.join(to, f), { recursive: true, force: true });
    return names;
  };
  const agentName = path.basename(P.agentExe);
  const built = await syncDir(dist, P.runtime, (f) => rust && (f === agentName || f.startsWith(`${agentName}.`)));
  info(`runtime -> ${fwd(P.runtime)} (${built.join(', ')})`);
  if (rust) info(`${path.basename(P.agentExe)}: ${await replaceFile(rustExe('agent-wiki'), P.agentExe)}`);

  const prev = await readJson(P.config).catch(() => null);
  const wikiDir = path.resolve(opt('--wiki-dir') || prev?.wikiDir || P.defaultWiki);
  if (prev?.wikiDir && path.resolve(prev.wikiDir) !== wikiDir) warn(`wiki folder changed from ${prev.wikiDir} (old folder left untouched)`);
  const port = Number(opt('--port') || prev?.httpPort || DEFAULT_PORT);
  const codexCli = which('codex');
  // Merge: settings you changed by hand (writeMode, curator.*, logs.*) are kept.
  const config = {
    ...(prev || {}),
    wikiDir: fwd(wikiDir),
    version: VERSION,
    httpPort: port,
    writeMode: prev?.writeMode || 'curated',
    curator: {
      model: 'gpt-6.1-sol',
      reasoningEffort: 'medium',
      ...(prev?.curator || {}),
      codexPath: fwd(prev?.curator?.codexPath || codexCli || 'codex'),
      codexHome: fwd(prev?.curator?.codexHome || P.curatorCodexHome),
    },
    logs: { retentionDays: 30, ...(prev?.logs || {}) },
  };
  await writeFileLF(P.config, toJson(config));
  info(`config -> ${fwd(P.config)} (writes: ${config.writeMode}; curator: ${config.curator.model}, ${config.curator.reasoningEffort})`);
  const init = rust ? mustRun('wiki init', P.agentExe, ['serve', '--init'], { env: cleanEnv() }) : mustRun('wiki init', nodePath(), [path.join(P.runtime, 'server.mjs'), '--init'], { env: cleanEnv() });
  const initRes = jsonFrom(init);
  if (!initRes?.ok || path.resolve(initRes.wikiDir) !== wikiDir) throw new InstallError(`wiki init returned ${init.stdout}`);
  info(`wiki -> ${fwd(wikiDir)}`);

  // 2b ------------------------------------------------------------------------
  step('2b', `Windows service ${SERVICE_NAME}`);
  let transport = 'stdio';
  const mcpUrl = `http://127.0.0.1:${port}/mcp`;
  if (process.platform === 'win32') {
    await fsp.mkdir(P.serviceDir, { recursive: true });
    await fsp.mkdir(P.logs, { recursive: true });
    const hashFile = `${P.serviceExe}.src-sha256`;
    const hash = serviceSourceHash();
    let rebuilt = false;
    if (rust) info(`service program: ${fwd(P.agentExe)} service`);
    else if (!exists(P.serviceExe) || (await fsp.readFile(hashFile, 'utf8').catch(() => '')).trim() !== hash) {
      rebuilt = true;
      const fresh = path.join(P.serviceDir, `AgentWikiService.new-${process.pid}.exe`);
      compileService(fresh);
      // A running exe cannot be overwritten but can be renamed; the new one is used from the next service start.
      if (exists(P.serviceExe)) await fsp.rename(P.serviceExe, path.join(P.serviceDir, `AgentWikiService.old-${Date.now()}.exe`));
      await fsp.rename(fresh, P.serviceExe);
      await writeFileLF(hashFile, `${hash}\n`);
      info(`service wrapper compiled -> ${fwd(P.serviceExe)}`);
    } else info('service wrapper up to date');
    for (const f of await fsp.readdir(P.serviceDir)) {
      if (/^AgentWikiService\.old-\d+\.exe$/.test(f)) await fsp.rm(path.join(P.serviceDir, f), { force: true }).catch(() => {});
    }
    await writeFileLF(
      P.serviceIni,
      serviceIni({ node: nodePath(), runtime: P.runtime, wikiDir, configDir: P.configDir, dataDir: P.dataDir, stateDir: P.stateDir, port, logDir: P.logs }),
    );
    let svc = serviceState();
    const bin = svc ? serviceBinPath() : null;
    if (svc) {
      // The service's account reads config.json and the runtime and writes its logs. These are your own
      // folders, so granting it access needs no admin rights (install-service grants the same).
      await fsp.mkdir(P.configDir, { recursive: true });
      for (const [target, perm] of [[wikiDir, 'M'], [P.configDir, 'RX'], [P.dataDir, 'RX'], [P.logs, 'M']]) {
        try {
          grantService(target, perm);
        } catch (e) {
          warn(e.message);
        }
      }
      info('service account access: Modify on the wiki and logs, Read on config and program files');
    }
    const wantBin = rust ? P.agentExe : P.serviceExe;
    if (bin && path.resolve(bin).toLowerCase() !== path.resolve(wantBin).toLowerCase()) {
      // The service still starts the wrapper from an earlier folder. Pointing it at this one, and giving its
      // account access to the new folders, needs administrator rights once: one UAC prompt.
      info(`the service starts ${fwd(bin)}; re-registering it at ${fwd(wantBin)} (approve the UAC prompt)`);
      const logFile = path.join(P.logs, 'install-service.log');
      await fsp.rm(logFile, { force: true });
      const r = runElevated(nodePath(), [path.join(REPO, 'scripts', 'install-service.mjs'), '--from-install-local', '--log', logFile, ...(rust ? [] : ['--node'])]);
      const out = (await fsp.readFile(logFile, 'utf8').catch(() => '')).trim();
      if (out) for (const line of out.split('\n')) info(`  | ${line}`);
      if (!r.ok) warn(`install-service did not finish (${r.error}). Run it in an elevated terminal: cd "${REPO}"; node scripts\\install-service.mjs`);
      svc = serviceState();
    } else if (svc === 'RUNNING' && rebuilt) {
      // The running wrapper is the old build (renamed above); a restart runs the new one. Your account may
      // stop and start this service (install-service granted it).
      info(`restarting the service ${SERVICE_NAME} so it runs the new wrapper`);
      run('sc.exe', ['stop', SERVICE_NAME]);
      for (let i = 0; i < 40 && serviceState() !== 'STOPPED'; i++) await new Promise((r) => setTimeout(r, 500));
      run('sc.exe', ['start', SERVICE_NAME]);
      for (let i = 0; i < 40 && serviceState() !== 'RUNNING'; i++) await new Promise((r) => setTimeout(r, 500));
      svc = serviceState();
    } else if (svc === 'STOPPED') {
      info(`starting the service ${SERVICE_NAME}${stoppedService ? ' again' : ''}`);
      run('sc.exe', ['start', SERVICE_NAME]);
      for (let i = 0; i < 40 && serviceState() !== 'RUNNING'; i++) await new Promise((r) => setTimeout(r, 500));
      svc = serviceState();
    }
    if (svc === 'RUNNING') {
      // The service restarts on a new build (the Rust program stops for the SCM to restart it; the C# wrapper
      // restarts node when server.mjs changes), so wait for exactly this build.
      const build = rust ? sha12(P.agentExe) : crypto.createHash('sha256').update(await fsp.readFile(path.join(P.runtime, 'server.mjs'))).digest('hex').slice(0, 12);
      const h = await waitForHealth(port, (x) => x?.ok && x.version === VERSION && x.build === build, 45_000);
      if (h?.ok && h.build === build && path.resolve(h.wikiDir) === wikiDir) {
        transport = 'http';
        info(`service running: v${h.version} (build ${h.build}), pid ${h.pid}, ${mcpUrl}`);
      } else warn(`service is RUNNING but /health returned ${JSON.stringify(h)}; staying on stdio. See ${fwd(P.logs)}/service.log`);
    } else {
      warn(`service ${svc ? `is ${svc}` : 'is not installed'}; clients will launch the server themselves (stdio).`);
      warn('To install it, run in an elevated terminal (Run as administrator):');
      warn(`  cd "${REPO}"; npm run install-service`);
    }
  }
  state.transport = transport;
  note('Server transport', transport === 'http' ? 'ok' : 'stdio', transport === 'http' ? `Windows service at ${mcpUrl}` : 'per-app stdio (service not running)');

  // 2c ------------------------------------------------------------------------
  step('2c', 'Curator: its own Codex home and sign-in');
  // The curator runs as you (hosted by the tray) through `codex exec`, in a separate CODEX_HOME so it loads none of
  // your plugins, hooks, MCP servers or ~/.codex/AGENTS.md. That home needs its own one-time `codex login`.
  await fsp.mkdir(config.curator.codexHome, { recursive: true });
  state.userSid = currentUserSid() || state.userSid;
  if (config.writeMode !== 'curated') {
    info('writeMode is "direct": notes are written immediately; the curator has nothing to do');
    note('Curator', 'off', 'writeMode "direct"');
  } else if (!codexCli && !prev?.curator?.codexPath) {
    warn('codex CLI not found: the curator cannot run. Notes will queue in the wiki inbox until it can.');
    note('Curator', 'manual', 'codex CLI not found');
  } else {
    const ls = run(config.curator.codexPath, ['login', 'status'], { env: { ...process.env, CODEX_HOME: config.curator.codexHome } });
    const signedIn = ls.ok && /logged in/i.test(ls.stdout + ls.stderr) && !/not logged in/i.test(ls.stdout + ls.stderr);
    info(`CODEX_HOME ${fwd(config.curator.codexHome)}: ${signedIn ? 'signed in' : 'not signed in yet'}`);
    note('Curator', signedIn ? 'ok' : 'click', signedIn ? `signed in (${config.curator.model}, ${config.curator.reasoningEffort})` : 'sign in once: tray icon > Curator > Sign in to ChatGPT for the curator...');
  }

  // 2d ------------------------------------------------------------------------
  step('2d', 'Tray app (starts at sign-in)');
  if (process.platform === 'win32' && rust) {
    await fsp.mkdir(P.trayDir, { recursive: true });
    buildIcons({ outDir: P.trayIcons, preview: false });
    // Stop whichever tray runs (the C# one or an earlier Rust one share the single-instance names).
    for (const exe of [P.rustTrayExe, P.trayExe]) {
      if (!exists(exe)) continue;
      const q = run(exe, ['--quit'], { timeout: 40_000 });
      if (!q.ok) warn(`the running tray (${path.basename(exe)}) did not quit (exit ${q.code}); end it from its menu, then run install-local again`);
    }
    info(`${path.basename(P.rustTrayExe)}: ${await replaceFile(rustExe('agent-wiki-tray'), P.rustTrayExe)}`);
    // A GNU cross build loads WebView2 from a DLL beside it (the MSVC build links it in).
    for (const f of await fsp.readdir(path.dirname(rustExe('agent-wiki-tray')))) {
      if (/^WebView2Loader\.dll$/i.test(f)) await replaceFile(path.join(path.dirname(rustExe('agent-wiki-tray')), f), path.join(P.trayDir, f));
    }
    await writeFileLF(
      P.rustTrayIni,
      rustTrayIni({ agent: P.agentExe, wikiDir, logDir: P.logs, icons: P.trayIcons, port, codex: config.curator.codexPath, codexHome: config.curator.codexHome, taskXml: P.trayTaskXml, webviewDir: P.webviewDir }),
    );
    // The C# tray and its Edge window profile are replaced: the window is a WebView2 view with its own small folder.
    for (const f of [P.trayExe, P.trayIni, `${P.trayExe}.src-sha256`]) await fsp.rm(f, { force: true }).catch(() => {});
    await removeOldCopies(P.trayExe);
    if (exists(P.uiProfile)) {
      for (const p of processesUnder(P.uiProfile)) run('taskkill.exe', ['/PID', String(p.pid), '/T', '/F']);
      await fsp.rm(P.uiProfile, { recursive: true, force: true, maxRetries: 5 }).then(
        () => info(`removed the old window's Edge profile ${fwd(P.uiProfile)}`),
        () => warn(`could not remove ${fwd(P.uiProfile)} (in use): delete it later`),
      );
    }
    const sid = state.userSid || currentUserSid();
    let task = 'not registered';
    if (!sid) warn('could not determine your SID: no logon task, only the Run key');
    else {
      await fsp.writeFile(P.trayTaskXml, taskXmlBytes(trayTaskXml({ exe: P.rustTrayExe, sid })));
      try {
        registerTask(TRAY_TASK, P.trayTaskXml);
      } catch (e) {
        warn(`${e.message.trim()}\n      (the Run key alone starts the tray)`);
      }
      const q = queryTask(TRAY_TASK);
      task = !q.exists ? 'missing' : !q.enabled ? 'disabled' : path.resolve(q.command).toLowerCase() === path.resolve(P.rustTrayExe).toLowerCase() ? 'on' : `starts ${q.command}`;
      info(`logon task "${TRAY_TASK}" (Task Scheduler, runs as you): ${task}`);
    }
    const runData = `"${P.rustTrayExe}" --from run`;
    const reg = run('reg.exe', ['add', RUN_KEY, '/v', TRAY_RUN_VALUE, '/t', 'REG_SZ', '/d', runData, '/f']);
    if (!reg.ok) warn(`could not write HKCU\\...\\Run\\${TRAY_RUN_VALUE}:\n${reg.stdout}${reg.stderr}`);
    const runOk = regValue(RUN_KEY, TRAY_RUN_VALUE) === runData;
    info(`HKCU\\...\\Run\\${TRAY_RUN_VALUE}: ${runOk ? runData : 'missing'}`);
    if (task !== 'on' && !runOk) throw new InstallError('Neither a logon task nor the Run key could be registered: the tray would not start at sign-in.');
    const child = spawn(P.rustTrayExe, ['--from', 'install'], { detached: true, stdio: 'ignore' });
    child.unref();
    await new Promise((r) => setTimeout(r, 1500));
    const st = run(P.rustTrayExe, ['--selftest'], { timeout: 30_000 });
    const trayState = st.stdout.match(/^state=(\w+)/m)?.[1];
    info(`tray started (pid ${child.pid}); it sees the service as: ${trayState ?? 'unknown'}; start at sign-in: ${st.stdout.match(/^autostart=(.*)$/m)?.[1] ?? 'unknown'}`);
    info(`Agent Wiki window: an embedded WebView2 view (data in ${fwd(P.webviewDir)})`);
    const icon = trayIconPlacement(P.rustTrayExe);
    info(`tray icon: ${icon.text}`);
    const starts = `starts at sign-in via ${[task === 'on' && 'logon task', runOk && 'HKCU Run'].filter(Boolean).join(' + ')}`;
    note('Tray app', trayState && trayState !== 'down' && task === 'on' && runOk ? 'ok' : 'check', `${fwd(P.rustTrayExe)}, ${starts}; state ${trayState ?? 'unknown'}`);
    if (icon.promoted !== true) note('Tray icon', 'click', `in the ^ overflow; to pin it: ${PIN_TRAY_ICON}`);
    state.tray = { exe: fwd(P.rustTrayExe), runValue: TRAY_RUN_VALUE, task: TRAY_TASK };
  } else if (process.platform === 'win32') {
    await fsp.mkdir(P.trayDir, { recursive: true });
    buildIcons({ outDir: P.trayIcons, preview: false });
    const hashFile = `${P.trayExe}.src-sha256`;
    const hash = traySourceHash(path.join(P.trayIcons, 'agent-wiki-healthy.ico'));
    if (!exists(P.trayExe) || (await fsp.readFile(hashFile, 'utf8').catch(() => '')).trim() !== hash) {
      const fresh = path.join(P.trayDir, `AgentWikiTray.new-${process.pid}.exe`);
      compileTray(fresh, path.join(P.trayIcons, 'agent-wiki-healthy.ico'));
      if (exists(P.trayExe)) await fsp.rename(P.trayExe, path.join(P.trayDir, `AgentWikiTray.old-${Date.now()}.exe`));
      await fsp.rename(fresh, P.trayExe);
      await writeFileLF(hashFile, `${hash}\n`);
      info(`tray compiled -> ${fwd(P.trayExe)}`);
    } else info('tray app up to date');
    await writeFileLF(
      P.trayIni,
      trayIni({ node: nodePath(), runtime: P.runtime, wikiDir, uiProfile: P.uiProfile, logDir: P.logs, icons: P.trayIcons, port, codex: config.curator.codexPath, codexHome: config.curator.codexHome, taskXml: P.trayTaskXml }),
    );
    // Restart the tray so it runs this version (it also restarts its curator).
    const quit = run(P.trayExe, ['--quit'], { timeout: 40_000 });
    if (!quit.ok) warn(`the running tray did not quit (exit ${quit.code}); end it from its menu, then run install-local again`);
    for (const f of await fsp.readdir(P.trayDir)) {
      if (/^AgentWikiTray\.old-\d+\.exe$/.test(f)) await fsp.rm(path.join(P.trayDir, f), { force: true }).catch(() => {});
    }
    // Two independent ways to start at sign-in, both per user: a logon task and the Run key. On
    // 2026-10-02 the Run value alone was removed overnight by something outside Agent Wiki.
    const sid = state.userSid || currentUserSid();
    let task = 'not registered';
    if (!sid) warn('could not determine your SID: no logon task, only the Run key');
    else {
      await fsp.writeFile(P.trayTaskXml, taskXmlBytes(trayTaskXml({ exe: P.trayExe, sid })));
      try {
        registerTask(TRAY_TASK, P.trayTaskXml);
      } catch (e) {
        warn(`${e.message.trim()}\n      (the Run key alone starts the tray)`);
      }
      const q = queryTask(TRAY_TASK);
      task = !q.exists ? 'missing' : !q.enabled ? 'disabled' : path.resolve(q.command).toLowerCase() === path.resolve(P.trayExe).toLowerCase() ? 'on' : `starts ${q.command}`;
      info(`logon task "${TRAY_TASK}" (Task Scheduler, runs as you): ${task}`);
    }
    const runData = `"${P.trayExe}" --from run`;
    const reg = run('reg.exe', ['add', RUN_KEY, '/v', TRAY_RUN_VALUE, '/t', 'REG_SZ', '/d', runData, '/f']);
    if (!reg.ok) warn(`could not write HKCU\\...\\Run\\${TRAY_RUN_VALUE}:\n${reg.stdout}${reg.stderr}`);
    const runOk = regValue(RUN_KEY, TRAY_RUN_VALUE) === runData;
    info(`HKCU\\...\\Run\\${TRAY_RUN_VALUE}: ${runOk ? runData : 'missing'}`);
    if (task !== 'on' && !runOk) throw new InstallError('Neither a logon task nor the Run key could be registered: the tray would not start at sign-in.');
    const child = spawn(P.trayExe, [], { detached: true, stdio: 'ignore' });
    child.unref();
    await new Promise((r) => setTimeout(r, 1500));
    const st = run(P.trayExe, ['--selftest'], { timeout: 30_000 });
    const trayState = st.stdout.match(/^state=(\w+)/m)?.[1];
    info(`tray started (pid ${child.pid}); it sees the service as: ${trayState ?? 'unknown'}; start at sign-in: ${st.stdout.match(/^autostart=(.*)$/m)?.[1] ?? 'unknown'}`);
    info(`Agent Wiki window: ${fwd(P.uiProfile)} (its own Edge profile) ${warmUiProfile()}`);
    const icon = trayIconPlacement(P.trayExe);
    info(`tray icon: ${icon.text}`);
    const starts = `starts at sign-in via ${[task === 'on' && 'logon task', runOk && 'HKCU Run'].filter(Boolean).join(' + ')}`;
    note('Tray app', trayState && trayState !== 'down' && task === 'on' && runOk ? 'ok' : 'check', `${fwd(P.trayExe)}, ${starts}; state ${trayState ?? 'unknown'}`);
    if (icon.promoted !== true) note('Tray icon', 'click', `in the ^ overflow; to pin it: ${PIN_TRAY_ICON}`);
    state.tray = { exe: fwd(P.trayExe), runValue: TRAY_RUN_VALUE, task: TRAY_TASK };
  }

  // 3 -------------------------------------------------------------------------
  step(3, 'Render plugin and local marketplaces');
  const map = {
    __NODE__: fwd(nodePath()),
    __RUNTIME__: fwd(P.runtime),
    __VERSION__: VERSION,
    __HOOK_COMMAND__: hookCommand(rust),
    __POINTER__: POINTER.trim(),
    __PROTOCOL__: PROTOCOL.trim(),
  };
  const stdioServer = rust ? { mcpServer: { command: fwd(P.agentExe), args: ['serve'] } } : {};
  const files = await renderPlugin(P.rendered, map, transport === 'http' ? { mcpServer: { type: 'http', url: mcpUrl } } : stdioServer);
  info(`plugin -> ${fwd(P.rendered)} (${files.length} files, ${transport === 'http' ? `MCP over HTTP ${mcpUrl}` : 'MCP over stdio'})`);
  info(`hook command: ${map.__HOOK_COMMAND__}`);
  const author = { name: 'willctl' };
  const description = 'Shared long-term memory across Claude, ChatGPT and other AI apps.';
  await writeFileLF(
    path.join(P.market, '.claude-plugin', 'marketplace.json'),
    toJson({
      name: CLAUDE_MARKET,
      owner: author,
      metadata: { description: 'Local marketplace for the Agent Wiki plugin' },
      plugins: [{ name: PLUGIN, source: './plugins/agent-wiki', description, version: VERSION, author }],
    }),
  );
  await writeFileLF(
    path.join(P.market, '.agents', 'plugins', 'marketplace.json'),
    toJson({
      name: CLAUDE_MARKET,
      interface: { displayName: 'Agent Wiki (local)' },
      plugins: [
        {
          name: PLUGIN,
          source: { source: 'local', path: './plugins/agent-wiki' },
          policy: { installation: 'INSTALLED_BY_DEFAULT', authentication: 'ON_INSTALL' },
          category: 'Productivity',
        },
      ],
    }),
  );
  const claude = which('claude');
  if (claude) {
    for (const target of [P.rendered, P.market]) {
      const v = run(claude, ['plugin', 'validate', target]);
      if (!v.ok) throw new InstallError(`claude plugin validate ${fwd(target)} failed:\n${v.stdout}${v.stderr}`);
    }
    info('claude plugin validate: plugin and marketplace OK');
  }

  // 4 -------------------------------------------------------------------------
  step(4, 'ChatGPT desktop / Codex: personal marketplace');
  const personalRel = `./${fwd(path.relative(HOME, P.rendered))}`;
  let personalName = 'personal';
  if (!exists(P.personalMarket)) markCreated(P.personalMarket);
  await editFile(P.personalMarket, (text) => {
    const m = text ? JSON.parse(text) : { name: 'personal', interface: { displayName: 'Personal' }, plugins: [] };
    personalName = m.name || 'personal';
    m.plugins = Array.isArray(m.plugins) ? m.plugins : [];
    const entry = {
      name: PLUGIN,
      source: { source: 'local', path: personalRel },
      policy: { installation: 'INSTALLED_BY_DEFAULT', authentication: 'ON_INSTALL' },
      category: 'Productivity',
    };
    const i = m.plugins.findIndex((p) => p?.name === PLUGIN);
    if (i >= 0) m.plugins[i] = entry;
    else m.plugins.push(entry);
    return toJson(m);
  });
  info(`${fwd(P.personalMarket)}: entry ${PLUGIN} -> ${personalRel}`);
  const codexId = `${PLUGIN}@${personalName}`;
  state.codexPluginId = codexId;
  const codex = which('codex');
  if (codex) {
    await backupOnce(P.codexConfig);
    const markets = run(codex, ['plugin', 'marketplace', 'list']);
    if (!markets.stdout.includes(personalName)) {
      throw new InstallError(`codex does not list the personal marketplace "${personalName}":\n${markets.stdout}${markets.stderr}`);
    }
    const add = run(codex, ['plugin', 'add', codexId]);
    if (!add.ok) throw new InstallError(`codex plugin add ${codexId} failed:\n${add.stdout}${add.stderr}`);
    info(`codex plugin add ${codexId}: ${add.stdout.trim().split('\n').pop()}`);
    const list = run(codex, ['plugin', 'list', '--json']);
    const listed = list.ok && list.stdout.includes(codexId);
    note('ChatGPT/Codex plugin', listed ? 'ok' : 'check', listed ? `${codexId} installed` : list.stdout + list.stderr);
  } else {
    warn('codex CLI not found: install the plugin from the ChatGPT app (Plugins > Personal).');
    note('ChatGPT/Codex plugin', 'manual', 'codex CLI not found');
  }

  // 5 -------------------------------------------------------------------------
  step(5, 'Claude Code plugin and Claude desktop');
  if (claude) {
    await backupOnce(P.claudeSettings);
    const mk = jsonFrom(run(claude, ['plugin', 'marketplace', 'list', '--json'])) || [];
    const known = (Array.isArray(mk) ? mk : []).find((m) => m?.name === CLAUDE_MARKET);
    let haveMarket = Boolean(known);
    const knownPath = known?.path || known?.installLocation;
    if (knownPath && path.resolve(knownPath).toLowerCase() !== path.resolve(P.market).toLowerCase()) {
      // Registered at an earlier location (before 1.3: ~/.agent-wiki/marketplace): register it again here.
      mustRun('claude plugin marketplace remove', claude, ['plugin', 'marketplace', 'remove', CLAUDE_MARKET]);
      info(`marketplace ${CLAUDE_MARKET} was registered at ${fwd(knownPath)}; registering it at ${fwd(P.market)}`);
      haveMarket = false;
    }
    if (haveMarket) mustRun('claude plugin marketplace update', claude, ['plugin', 'marketplace', 'update', CLAUDE_MARKET]);
    else mustRun('claude plugin marketplace add', claude, ['plugin', 'marketplace', 'add', P.market]);
    info(`marketplace ${CLAUDE_MARKET} ${haveMarket ? 'updated' : 'added'}`);
    const installed = JSON.stringify(jsonFrom(run(claude, ['plugin', 'list', '--json'])) || []).includes(CLAUDE_PLUGIN_ID);
    if (installed) {
      const u = run(claude, ['plugin', 'update', CLAUDE_PLUGIN_ID]);
      info(`claude plugin update: ${(u.stdout + u.stderr).trim().split('\n').pop()}`);
      run(claude, ['plugin', 'enable', CLAUDE_PLUGIN_ID]);
    } else {
      mustRun('claude plugin install', claude, ['plugin', 'install', CLAUDE_PLUGIN_ID]);
      info(`installed ${CLAUDE_PLUGIN_ID}`);
    }
    const listed = () => {
      const all = jsonFrom(run(claude, ['plugin', 'list', '--json'])) || [];
      return (Array.isArray(all) ? all : all.plugins || []).find((p) => JSON.stringify(p).includes(CLAUDE_PLUGIN_ID));
    };
    // Claude's cache is keyed by version, so a re-render at the same version (e.g. stdio -> HTTP) is not picked up
    // by `plugin update`. Reinstall whenever the cached copy differs from what was just rendered.
    let mine = listed();
    if (mine?.installPath && (await treeHash(mine.installPath)) !== (await treeHash(P.rendered))) {
      mustRun('claude plugin uninstall', claude, ['plugin', 'uninstall', CLAUDE_PLUGIN_ID]);
      mustRun('claude plugin install', claude, ['plugin', 'install', CLAUDE_PLUGIN_ID]);
      mine = listed();
      const same = mine?.installPath && (await treeHash(mine.installPath)) === (await treeHash(P.rendered));
      if (!same) throw new InstallError(`Claude's cached plugin at ${mine?.installPath} still differs from ${fwd(P.rendered)}`);
      info('cached copy was stale: reinstalled the plugin');
    }
    note('Claude Code plugin', mine ? 'ok' : 'check', mine ? JSON.stringify(mine) : 'not listed');
  } else {
    warn('claude CLI not found: skipped the Claude Code plugin.');
    note('Claude Code plugin', 'manual', 'claude CLI not found');
  }

  const desktopConfigs = claudeDesktopConfigs();
  if (!desktopConfigs.length) note('Claude desktop MCP', 'manual', 'no Claude desktop config folder found');
  for (const cfg of desktopConfigs) {
    if (!exists(cfg)) markCreated(cfg);
    const before = await readJson(cfg).catch(() => null);
    if ((state.claudeDesktopConfigs || []).includes(fwd(cfg)) && !before?.mcpServers?.[SERVER]) {
      warn('Claude desktop removed the agent-wiki entry since the last install. On this account "Allow user-added MCP servers"');
      warn('appears to be off (an organization setting), so desktop Chat ignores and strips local servers. Re-adding it anyway.');
    }
    await editFile(cfg, (text) => {
      const c = text ? JSON.parse(text) : {};
      c.mcpServers ||= {};
      c.mcpServers[SERVER] = rust ? { command: fwd(P.agentExe), args: ['serve'] } : { command: fwd(nodePath()), args: [`${fwd(P.runtime)}/server.mjs`] };
      return toJson(c);
    });
    info(`registered ${SERVER} in ${fwd(cfg)}`);
    note('Claude desktop MCP', 'ok', fwd(cfg));
  }
  state.claudeDesktopConfigs = desktopConfigs.map(fwd);

  // 6 -------------------------------------------------------------------------
  step(6, 'Global instructions');
  for (const file of [P.codexAgents, P.claudeMd]) {
    if (!exists(file)) markCreated(file);
    const changed = await editFile(file, (text) => upsertBlock(text, POINTER));
    info(`${fwd(file)}: managed block ${changed ? 'written' : 'already current'}`);
  }

  // 7 -------------------------------------------------------------------------
  step(7, 'Tool approval: ChatGPT/Codex and Claude Code');
  if (flag('--no-approve')) {
    const changed = await editFile(P.codexConfig, (t) => tomlRemoveApproval(t, codexId));
    info(changed ? 'removed ChatGPT/Codex auto-approval' : 'ChatGPT/Codex auto-approval not set (--no-approve)');
    const cc = await editFile(P.claudeSettings, claudeAllowRemove);
    info(cc ? `removed ${CLAUDE_ALLOW.join(', ')} from ${fwd(P.claudeSettings)}` : 'Claude Code allow rules not set (--no-approve)');
  } else {
    const changed = await editFile(P.codexConfig, (t) => tomlSetApproval(t, codexId));
    info(`${fwd(P.codexConfig)}: [plugins."${codexId}".mcp_servers.${SERVER}] default_tools_approval_mode = "approve" (${changed ? 'written' : 'already set'})`);
    // Without these, Claude Code asks before every wiki call (and `claude -p` sessions are simply denied).
    const cc = await editFile(P.claudeSettings, claudeAllowSet);
    info(`${fwd(P.claudeSettings)}: permissions.allow has ${CLAUDE_ALLOW.join(', ')} (${cc ? 'written' : 'already set'})`);
  }
  if (codex) {
    const check = run(codex, ['plugin', 'list', '--json']);
    if (!check.ok) throw new InstallError(`codex cannot load config.toml after the edit; restore ${fwd(P.codexConfig)}.bak-agent-wiki:\n${check.stderr}`);
    const toml = await fsp.readFile(P.codexConfig, 'utf8');
    const mode = tomlGet(toml, 'plugins', codexId, 'mcp_servers', SERVER, 'default_tools_approval_mode');
    info(`codex still loads config.toml; approval mode = ${mode ?? '(unset)'}`);
  }

  // 8 -------------------------------------------------------------------------
  step(8, 'Paste-in text for app settings');
  await writeFileLF(P.paste, `${POINTER.trim()}\n`);
  const clip = run('powershell.exe', [
    '-NoProfile',
    '-NonInteractive',
    '-Command',
    `Set-Clipboard -Value (Get-Content -Raw -Encoding UTF8 -LiteralPath '${P.paste.replace(/'/g, "''")}')`,
  ]);
  info(`${fwd(P.paste)} written${clip.ok ? ' and copied to the clipboard' : ` (clipboard copy failed: ${clip.stderr.trim()})`}`);

  // 9 -------------------------------------------------------------------------
  step(9, 'Self-test of the installed server and hook');
  const transports = [
    ['stdio', () => (rust ? new StdioClientTransport({ command: P.agentExe, args: ['serve'], env: cleanEnv(), stderr: 'pipe' }) : new StdioClientTransport({ command: nodePath(), args: [path.join(P.runtime, 'server.mjs')], env: cleanEnv(), stderr: 'pipe' }))],
  ];
  if (transport === 'http') transports.push(['HTTP service', () => new StreamableHTTPClientTransport(new URL(mcpUrl))]);
  for (const [label, make] of transports) {
    const client = new Client({ name: 'agent-wiki-installer', version: VERSION });
    await client.connect(make());
    try {
      const sv = client.getServerVersion();
      const { tools } = await client.listTools();
      const names = tools.map((t) => t.name).sort().join(',');
      if (names !== 'wiki_log,wiki_read,wiki_search,wiki_start,wiki_upsert_page') throw new InstallError(`unexpected tools: ${names}`);
      if (!client.getInstructions()?.includes(fwd(wikiDir))) throw new InstallError('server instructions do not name the wiki folder');
      const r = await client.callTool({ name: 'wiki_start', arguments: { app: 'installer', topic: 'install' } });
      if (r.isError) throw new InstallError(`wiki_start failed over ${label}: ${r.content[0]?.text}`);
      info(`MCP over ${label} OK: ${sv.name} ${sv.version}, 5 tools, wiki_start OK`);
    } finally {
      await client.close();
    }
  }
  const hookCmd = map.__HOOK_COMMAND__;
  const shells = [
    ['cmd.exe', ['/d', '/s', '/c', `"${hookCmd}"`], { windowsVerbatimArguments: true }],
    ['powershell.exe', ['-NoProfile', '-NonInteractive', '-Command', hookCmd]],
    [which('pwsh'), ['-NoProfile', '-NonInteractive', '-Command', hookCmd]],
    [exists('C:/Program Files/Git/bin/bash.exe') ? 'C:/Program Files/Git/bin/bash.exe' : null, ['-c', hookCmd]],
  ].filter(([s]) => s);
  for (const [shell, shellArgs, extra] of shells) {
    const h = run(shell, shellArgs, { env: cleanEnv(), timeout: 20_000, ...extra });
    let ctx = null;
    try {
      ctx = JSON.parse(h.stdout).hookSpecificOutput.additionalContext;
    } catch {
      // reported below
    }
    if (!h.ok || !ctx?.startsWith('Agent Wiki is the user')) {
      throw new InstallError(`hook command failed under ${path.basename(shell)}: exit ${h.code}\n${h.stdout}\n${h.stderr}`);
    }
    info(`hook OK under ${path.basename(shell)}`);
  }

  // 10 ------------------------------------------------------------------------
  if (migrated || exists(P.legacyHome)) {
    step(10, `Remove ${fwd(P.legacyHome)}`);
    // Anything an app wrote there since step 1b (a session still running the old server) is merged first.
    const res = await migrate({ legacy: P.legacyHome, items, journal: P.migration, log: info });
    if (res.removedLegacy) info(`${fwd(P.legacyHome)} is gone`);
    else warn(`${fwd(P.legacyHome)} stays: ${[...res.conflicts, ...res.unknown].join('; ')}`);
    const old = processesUnder(P.legacyHome);
    if (old.length) {
      warn(`still running from the old location (they write their logs there until restarted):\n${describeProcesses(old)}`);
      note('Old location', 'click', 'restart the apps above (Claude desktop, ChatGPT, open sessions), then from cmd: node scripts/migrate.mjs --run');
    }
  }

  state.version = VERSION;
  state.installedAt = new Date().toISOString();
  state.wikiDir = fwd(wikiDir);
  await saveState(state);

  console.log('\nInstalled.');
  for (const r of results) console.log(`  ${r.status.padEnd(6)} ${r.area}: ${r.detail.length > 160 ? `${r.detail.slice(0, 157)}...` : r.detail}`);
  console.log(`\n  Wiki:    ${fwd(wikiDir)}`);
  console.log(`  Runtime: ${fwd(P.runtime)}${rust ? ` (Rust: ${path.basename(P.agentExe)}, ${path.basename(P.rustTrayExe)})` : ' (Node)'}`);
  console.log(`  Server:  ${transport === 'http' ? `Windows service ${SERVICE_NAME} at ${mcpUrl}` : 'launched by each app over stdio'}`);
  if (transport !== 'http' && process.platform === 'win32') {
    console.log(`\n  To run it as a Windows service: elevated terminal -> cd "${REPO}"; npm run install-service`);
    console.log('  then run npm run install-local again (normal terminal) to point the apps at it.');
  }
  console.log('\n  Restart the Claude and ChatGPT desktop apps to load the plugin and server.');
  console.log('  In ChatGPT, accept the Agent Wiki hook trust prompt when it appears.');
  console.log(`  Paste ${fwd(P.paste)} (now on your clipboard) into each app's personal instructions.`);
}

main().catch((e) => {
  console.error(`\nInstall failed: ${e instanceof InstallError ? e.message : e?.stack || e}`);
  process.exit(1);
});
