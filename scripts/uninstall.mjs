// npm run uninstall-local
//
// Stops the tray app (and with it the curator) and removes its logon task, Run
// key entry and files (install step 2d), then reverses install steps 3-8: rendered plugin and
// marketplaces, the ChatGPT / Codex plugin, the Claude Code plugin and Claude
// desktop registration, the global instruction blocks, the ChatGPT approval
// setting and the paste file.
// Never deletes the wiki. Leaves the runtime (on Windows %LOCALAPPDATA%\AgentWiki) and
// config.json (%APPDATA%\AgentWiki) in place; delete those folders by hand to remove them too.

import fsp from 'node:fs/promises';
import path from 'node:path';
import {
  CLAUDE_MARKET,
  CLAUDE_PLUGIN_ID,
  HOME,
  InstallError,
  appDataVirtualized,
  P,
  REPO,
  PLUGIN,
  RUN_KEY,
  SERVER,
  TRAY_RUN_VALUE,
  TRAY_TASK,
  CLAUDE_ALLOW,
  claudeAllowRemove,
  claudeDesktopConfigs,
  deleteTask,
  editFile,
  exists,
  fwd,
  info,
  loadState,
  readJson,
  readText,
  removeBlock,
  removeEmptyDirs,
  run,
  saveState,
  step,
  toJson,
  tomlRemoveApproval,
  warn,
  which,
} from './lib.mjs';

async function main() {
  const pkg = appDataVirtualized();
  if (pkg) {
    throw new InstallError(
      `This terminal runs inside the app package ${pkg}, where Windows redirects AppData writes into that app's private` +
        ` storage. Run it from a normal terminal: Win+R, cmd, then: cd /d "${REPO}" && npm run uninstall-local`,
    );
  }
  const state = await loadState();
  const created = new Set(state.created || []);
  const codexId = state.codexPluginId || `${PLUGIN}@personal`;
  const cfg = await readJson(P.config).catch(() => null);
  console.log('Agent Wiki uninstaller');

  step('2d', 'Tray app and its start at sign-in');
  for (const exe of [P.rustTrayExe, P.trayExe].filter(exists)) {
    const q = run(exe, ['--quit'], { timeout: 40_000 });
    info(q.ok ? 'tray stopped (its curator stopped with it)' : `tray --quit: exit ${q.code}`);
  }
  const taskName = state.tray?.task || TRAY_TASK;
  info(deleteTask(taskName) ? `removed the logon task "${taskName}"` : 'no logon task');
  const reg = run('reg.exe', ['delete', RUN_KEY, '/v', TRAY_RUN_VALUE, '/f']);
  info(reg.ok ? `removed HKCU\\...\\Run\\${TRAY_RUN_VALUE}` : 'no Run key entry');
  for (let i = 0; i < 10; i++) {
    try {
      await fsp.rm(P.trayDir, { recursive: true, force: true });
      break;
    } catch {
      await new Promise((r) => setTimeout(r, 500)); // the exe stays locked for a moment after it exits
    }
  }
  info(`removed ${fwd(P.trayDir)}`);
  // The Agent Wiki window's own Edge profile (browser cache only). Edge may hold it while that window is open.
  await fsp.rm(P.uiProfile, { recursive: true, force: true, maxRetries: 5 }).then(
    () => info(`removed ${fwd(P.uiProfile)}`),
    () => warn(`could not remove ${fwd(P.uiProfile)}: close the Agent Wiki window, then delete it`),
  );
  if (exists(P.curatorCodexHome)) {
    info(`kept the curator's Codex sign-in in ${fwd(P.curatorCodexHome)} (delete the folder to sign it out for good)`);
  }

  step(3, 'Rendered plugin and local marketplaces');
  const claude = which('claude');
  if (claude) {
    const u = run(claude, ['plugin', 'uninstall', CLAUDE_PLUGIN_ID]);
    info(`claude plugin uninstall: ${u.ok ? 'done' : (u.stdout + u.stderr).trim().split('\n').pop()}`);
    const m = run(claude, ['plugin', 'marketplace', 'remove', CLAUDE_MARKET]);
    info(`claude plugin marketplace remove: ${m.ok ? 'done' : (m.stdout + m.stderr).trim().split('\n').pop()}`);
  }
  const codex = which('codex');
  if (codex) {
    const r = run(codex, ['plugin', 'remove', codexId]);
    info(`codex plugin remove ${codexId}: ${r.ok ? 'done' : (r.stdout + r.stderr).trim().split('\n').pop()}`);
  }
  await fsp.rm(P.market, { recursive: true, force: true });
  info(`removed ${fwd(P.market)}`);

  step(4, 'ChatGPT personal marketplace entry');
  let emptied = false;
  await editFile(P.personalMarket, (text) => {
    if (!text) return null;
    const m = JSON.parse(text);
    const before = (m.plugins || []).length;
    m.plugins = (m.plugins || []).filter((p) => p?.name !== PLUGIN);
    if (m.plugins.length === before) return null;
    emptied = m.plugins.length === 0;
    return toJson(m);
  });
  if (emptied && created.has(fwd(P.personalMarket))) {
    await fsp.rm(P.personalMarket, { force: true });
    await removeEmptyDirs(path.dirname(P.personalMarket), HOME);
    info(`deleted ${fwd(P.personalMarket)} (the installer created it)`);
  } else info(`${fwd(P.personalMarket)}: entry removed`);

  step(5, 'Claude desktop registration');
  const desktop = new Set([...(state.claudeDesktopConfigs || []), ...claudeDesktopConfigs().map(fwd)]);
  for (const file of desktop) {
    const changed = await editFile(file, (text) => {
      if (!text) return null;
      const c = JSON.parse(text);
      if (!c.mcpServers?.[SERVER]) return null;
      delete c.mcpServers[SERVER];
      if (!Object.keys(c.mcpServers).length) delete c.mcpServers;
      return toJson(c);
    });
    if (changed) info(`removed ${SERVER} from ${file}`);
  }

  step(6, 'Global instruction blocks');
  for (const file of [P.codexAgents, P.claudeMd]) {
    const text = await readText(file);
    const next = removeBlock(text?.replace(/\r\n/g, '\n'));
    if (next === null) continue;
    if (next === '' && created.has(fwd(file))) {
      await fsp.rm(file, { force: true });
      info(`deleted ${fwd(file)} (the installer created it)`);
    } else {
      await editFile(file, () => next);
      info(`${fwd(file)}: block removed`);
    }
  }

  step(7, 'Tool approval: ChatGPT/Codex and Claude Code');
  const changed = await editFile(P.codexConfig, (t) => tomlRemoveApproval(t, codexId));
  info(changed ? `removed [plugins."${codexId}".mcp_servers.${SERVER}]` : 'nothing to remove in config.toml');
  const cc = await editFile(P.claudeSettings, claudeAllowRemove);
  info(cc ? `removed ${CLAUDE_ALLOW.join(', ')} from ${fwd(P.claudeSettings)}` : 'no Claude Code allow rules to remove');
  if (codex && !run(codex, ['plugin', 'list', '--json']).ok) warn('codex could not load config.toml afterwards; check it');

  step(8, 'Paste file');
  await fsp.rm(P.paste, { force: true });

  state.uninstalledAt = new Date().toISOString();
  state.created = [...created].filter((f) => exists(f));
  await saveState(state);
  console.log('\nUninstalled. Backups (*.bak-agent-wiki) were kept.');
  console.log(`Your wiki is untouched at: ${cfg?.wikiDir || fwd(P.defaultWiki)}`);
  console.log(`Runtime and config remain in ${fwd(P.dataDir)} and ${fwd(P.configDir)} (delete them to remove them).`);
}

main().catch((e) => {
  console.error(`\nUninstall failed: ${e instanceof InstallError ? e.message : e?.stack || e}`);
  process.exit(1);
});
