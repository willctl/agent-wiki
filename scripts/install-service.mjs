// npm run install-service   (run in an ELEVATED terminal: "Run as administrator")
//
// Registers the AgentWiki Windows service: `agent-wiki service` (the Rust program installed by
// `npm run install-local`) serves HTTP on 127.0.0.1; with the Node runtime (install-local --node),
// AgentWikiService.exe supervises `node server.mjs --http` instead.
// It starts at boot, restarts on failure, and runs as the low-privilege
// virtual account NT SERVICE\AgentWiki, which gets Modify on the wiki folder,
// Read on Agent Wiki's settings and program folders (%APPDATA%\AgentWiki,
// %LOCALAPPDATA%\AgentWiki), and Modify on its logs folder. Nothing else.
// It also lets YOUR account start and stop this one service, so the tray app
// can restart it (step 2b). Re-running updates the service in place.
//   --user-sid S-1-...    grant start/stop to this account instead of the one install-local recorded
//   --log <file>          also write the output to <file> (install-local runs this elevated, hidden, and shows it)
//   --from-install-local  install-local started it: re-pointing the service at a new folder (one UAC prompt)

import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import {
  InstallError,
  appDataVirtualized,
  P,
  REPO,
  SERVICE_ACCOUNT,
  SERVICE_NAME,
  TRAY_SERVICE_RIGHTS,
  currentUserSid,
  exists,
  fwd,
  info,
  isElevated,
  loadState,
  readJson,
  run,
  sddlWithStartStop,
  serviceState,
  step,
  waitForHealth,
  warn,
} from './lib.mjs';

const args = process.argv.slice(2);
const opt = (n) => {
  const i = args.indexOf(n);
  return i >= 0 ? args[i + 1] : undefined;
};

function sc(...args) {
  const r = run('sc.exe', args);
  if (!r.ok) throw new InstallError(`sc.exe ${args.join(' ')} failed (exit ${r.code}):\n${r.stdout}${r.stderr}`);
  return r.stdout;
}

function grant(target, perm) {
  const r = run('icacls.exe', [target, '/grant', `${SERVICE_ACCOUNT}:(OI)(CI)${perm}`, '/Q']);
  if (!r.ok) throw new InstallError(`icacls ${target} failed:\n${r.stdout}${r.stderr}`);
  info(`granted ${SERVICE_ACCOUNT} ${perm === 'M' ? 'Modify' : 'Read'} on ${fwd(target)}`);
}

async function waitForState(want, timeoutMs = 20_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (serviceState() === want) return true;
    await new Promise((r) => setTimeout(r, 300));
  }
  return false;
}

const logFile = opt('--log');
if (logFile) {
  fs.mkdirSync(path.dirname(logFile), { recursive: true });
  for (const k of ['log', 'error']) {
    const orig = console[k].bind(console);
    console[k] = (...a) => {
      orig(...a);
      fs.appendFileSync(logFile, `${a.join(' ')}\n`);
    };
  }
}

async function main() {
  if (process.platform !== 'win32') throw new InstallError('The service is Windows-only.');
  const pkg = appDataVirtualized();
  if (pkg) {
    throw new InstallError(
      `This terminal runs inside the app package ${pkg}, where Windows redirects AppData writes into that app's private` +
        ` storage. Run it from an elevated normal terminal: Win+R, cmd (Ctrl+Shift+Enter to run it as administrator), then: cd /d "${REPO}" && npm run install-service`,
    );
  }
  if (!isElevated()) {
    throw new InstallError('This needs administrator rights. Open a terminal with "Run as administrator", then:\n' +
      `  cd "${REPO}"\n  npm run install-service`);
  }
  const cfg = await readJson(P.config);
  const rust = exists(P.agentExe) && !args.includes('--node');
  if (!cfg?.wikiDir || !(rust || exists(P.serviceExe)) || !exists(P.serviceIni)) {
    throw new InstallError(`Run \`npm run install-local\` in a normal terminal first (missing ${fwd(P.config)}, ${fwd(P.serviceIni)} or the service program).`);
  }
  const wikiDir = path.resolve(cfg.wikiDir);
  const port = cfg.httpPort;
  console.log(`Agent Wiki service installer: ${SERVICE_NAME} as ${SERVICE_ACCOUNT}, port ${port}`);

  step(1, 'Register the service');
  const binPath = rust ? `"${P.agentExe}" service --config "${P.serviceIni}"` : `"${P.serviceExe}"`;
  info(`program: ${binPath}`);
  const existing = serviceState();
  if (existing) {
    if (existing !== 'STOPPED') {
      run('sc.exe', ['stop', SERVICE_NAME]);
      if (!(await waitForState('STOPPED'))) warn('service did not report STOPPED within 20 s');
    }
    sc('config', SERVICE_NAME, 'binPath=', binPath, 'start=', 'auto', 'obj=', SERVICE_ACCOUNT, 'DisplayName=', 'Agent Wiki');
    info(`updated existing service (${existing})`);
  } else {
    sc('create', SERVICE_NAME, 'binPath=', binPath, 'start=', 'auto', 'obj=', SERVICE_ACCOUNT, 'DisplayName=', 'Agent Wiki');
    info('created service');
  }
  sc('description', SERVICE_NAME, 'Shared AI memory (agent-wiki MCP server) on http://127.0.0.1 for Claude, ChatGPT and other local AI apps.');
  // The Rust service also stops with an error code when an install replaces its program, so these
  // restarts are how an upgrade takes effect without admin rights.
  sc('failure', SERVICE_NAME, 'reset=', '86400', 'actions=', 'restart/2000/restart/5000/restart/30000');
  sc('failureflag', SERVICE_NAME, '1');
  info('start: automatic at boot; on failure or upgrade: restart after 2 s, 5 s, 30 s');

  step(2, 'Folder access for the service account');
  await fsp.mkdir(P.logs, { recursive: true });
  grant(wikiDir, 'M');
  await fsp.mkdir(P.configDir, { recursive: true });
  grant(P.configDir, 'RX');
  grant(P.dataDir, 'RX');
  grant(P.logs, 'M');

  step('2b', 'Let your account start and stop this service (the tray\'s "Restart service")');
  // One ACE on THIS service's security descriptor: SERVICE_START (RP), SERVICE_STOP (WP) and
  // SERVICE_QUERY_STATUS (LC) for your SID. Not config change, delete or permission change, and no
  // other service. The SID comes from `install-local` (run as you, not elevated), so running this
  // script from another admin account still grants it to you.
  const state = await loadState();
  const sid = opt('--user-sid') || state.userSid || currentUserSid();
  if (!sid) throw new InstallError('Could not determine your account SID; run `npm run install-local` first, or pass --user-sid S-1-...');
  const before = sc('sdshow', SERVICE_NAME).trim();
  const after = sddlWithStartStop(before, sid);
  if (after === before) info(`already granted: ${sid} may start/stop ${SERVICE_NAME}`);
  else {
    sc('sdset', SERVICE_NAME, after);
    info(`granted ${sid} start/stop/query on ${SERVICE_NAME} only (ACE (A;;${TRAY_SERVICE_RIGHTS};;;${sid}))`);
  }
  info(`security descriptor now: ${sc('sdshow', SERVICE_NAME).trim()}`);

  step(3, 'Start and check health');
  sc('start', SERVICE_NAME);
  if (!(await waitForState('RUNNING'))) warn('service did not report RUNNING within 20 s');
  const h = await waitForHealth(port);
  if (!h?.ok) {
    const tail = (await fsp.readFile(path.join(P.logs, 'service.log'), 'utf8').catch(() => '')).split('\n').slice(-15).join('\n');
    throw new InstallError(`The service is not healthy: ${JSON.stringify(h)}\nLast log lines (${fwd(P.logs)}/service.log):\n${tail}`);
  }
  info(`healthy: agent-wiki v${h.version}, pid ${h.pid}, wiki ${h.wikiDir}, http://127.0.0.1:${port}/mcp`);

  if (args.includes('--from-install-local')) {
    console.log('Service installed.');
    return;
  }
  console.log('\nService installed. Now, in a NORMAL (non-admin) terminal:');
  console.log(`  cd "${REPO}"\n  npm run install-local`);
  console.log('That points Claude Code and ChatGPT at the service. Then restart both desktop apps.');
}

main().catch((e) => {
  console.error(`\nService install failed: ${e instanceof InstallError ? e.message : e?.stack || e}`);
  process.exit(1);
});
