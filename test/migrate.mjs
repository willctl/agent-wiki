// Standard locations (src/paths.mjs) and the move off a pre-1.3 ~/.agent-wiki (scripts/migrate.mjs).

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { describe, test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { appPaths, pathsEnv } from '../src/paths.mjs';
import { mergeDir, migrate, migrationItems, packagedCopies, planMigration, remapper, rollback } from '../scripts/migrate.mjs';
import { srv } from './impl.mjs';

const REPO = fileURLToPath(new URL('..', import.meta.url));
const fwd = (p) => p.replace(/\\/g, '/');

describe('standard locations', () => {
  test('Windows: settings in Roaming, everything else in Local', () => {
    const p = appPaths({ env: { APPDATA: 'C:\\U\\AppData\\Roaming', LOCALAPPDATA: 'C:\\U\\AppData\\Local' }, platform: 'win32', home: 'C:\\U' });
    assert.equal(p.mode, 'standard');
    assert.equal(p.configFile, path.join('C:\\U\\AppData\\Roaming', 'AgentWiki', 'config.json'));
    assert.equal(p.runtimeDir, path.join('C:\\U\\AppData\\Local', 'AgentWiki', 'runtime'));
    assert.equal(p.logDir, path.join('C:\\U\\AppData\\Local', 'AgentWiki', 'logs'));
    assert.equal(p.installState, path.join('C:\\U\\AppData\\Local', 'AgentWiki', 'state', 'install-state.json'));
    assert.equal(p.uiProfile, path.join('C:\\U\\AppData\\Local', 'AgentWiki', 'ui-profile'));
    assert.equal(p.httpSessions, path.join(p.logDir, 'http-sessions.json'), 'the service writes only logs/');
    assert.equal(p.legacyHome, path.join('C:\\U', '.agent-wiki'));
  });

  test('Linux: XDG directories, defaults when unset, relative values ignored', () => {
    const d = appPaths({ env: {}, platform: 'linux', home: '/home/w' });
    assert.equal(d.configDir, path.join('/home/w', '.config', 'agent-wiki'));
    assert.equal(d.dataDir, path.join('/home/w', '.local', 'share', 'agent-wiki'));
    assert.equal(d.stateDir, path.join('/home/w', '.local', 'state', 'agent-wiki'));
    assert.equal(d.logDir, path.join('/home/w', '.local', 'state', 'agent-wiki', 'logs'));
    assert.equal(d.uiProfile, path.join('/home/w', '.cache', 'agent-wiki', 'ui-profile'));
    const x = appPaths({ env: { XDG_CONFIG_HOME: '/cfg', XDG_DATA_HOME: 'relative/data', XDG_STATE_HOME: '/st', XDG_CACHE_HOME: '/c' }, platform: 'linux', home: '/home/w' });
    assert.equal(x.configDir, path.join('/cfg', 'agent-wiki'));
    assert.equal(x.dataDir, path.join('/home/w', '.local', 'share', 'agent-wiki'), 'a relative XDG path is ignored');
    assert.equal(x.logDir, path.join('/st', 'agent-wiki', 'logs'));
    assert.equal(x.cacheDir, path.join('/c', 'agent-wiki'));
  });

  test('macOS: ~/Library, with an XDG variable winning for its kind', () => {
    const m = appPaths({ env: {}, platform: 'darwin', home: '/Users/w' });
    assert.equal(m.configFile, path.join('/Users/w', 'Library', 'Application Support', 'Agent Wiki', 'config.json'));
    assert.equal(m.logDir, path.join('/Users/w', 'Library', 'Logs', 'Agent Wiki'));
    assert.equal(m.cacheDir, path.join('/Users/w', 'Library', 'Caches', 'Agent Wiki'));
    const x = appPaths({ env: { XDG_CONFIG_HOME: '/Users/w/.config' }, platform: 'darwin', home: '/Users/w' });
    assert.equal(x.configDir, path.join('/Users/w/.config', 'agent-wiki'));
    assert.equal(x.dataDir, path.join('/Users/w', 'Library', 'Application Support', 'Agent Wiki'));
  });

  test('AGENT_WIKI_HOME is one portable folder; per-kind overrides win; pathsEnv round-trips', () => {
    const port = appPaths({ env: { AGENT_WIKI_HOME: '/tmp/h' }, platform: 'linux', home: '/home/w' });
    assert.equal(port.mode, 'portable');
    assert.equal(port.configFile, path.resolve('/tmp/h', 'config.json'));
    assert.equal(port.logDir, path.resolve('/tmp/h', 'logs'));
    assert.equal(port.curatorCodexHome, path.resolve('/tmp/h', 'curator', 'codex-home'));
    const over = appPaths({ env: { AGENT_WIKI_HOME: '/tmp/h', AGENT_WIKI_LOG_DIR: '/var/log/aw' }, platform: 'linux', home: '/home/w' });
    assert.equal(over.logDir, path.resolve('/var/log/aw'));
    const again = appPaths({ env: pathsEnv(over), platform: 'win32', home: 'C:\\other' });
    for (const k of ['configDir', 'dataDir', 'stateDir', 'logDir', 'cacheDir']) assert.equal(again[k], over[k], k);
  });

  test('the runtime reads config.json from the standard place, or the old ~/.agent-wiki until it moves', async () => {
    const tmp = fs.realpathSync(await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-paths-')));
    try {
      const home = path.join(tmp, 'home');
      const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !/^(AGENT_WIKI_|XDG_)/.test(k)));
      Object.assign(env, { USERPROFILE: home, HOME: home, APPDATA: path.join(home, 'AppData', 'Roaming'), LOCALAPPDATA: path.join(home, 'AppData', 'Local') });
      const p = appPaths({ env, home });
      const init = () => {
        const r = spawnSync(...srv(path.join(REPO, 'dist', 'runtime'), '--init'), { env, encoding: 'utf8' });
        return JSON.parse(r.stdout.slice(r.stdout.indexOf('{'))).wikiDir;
      };
      fs.mkdirSync(p.legacyHome, { recursive: true });
      fs.writeFileSync(path.join(p.legacyHome, 'config.json'), JSON.stringify({ wikiDir: fwd(path.join(tmp, 'old-wiki')) }));
      assert.equal(path.resolve(init()), path.join(tmp, 'old-wiki'), 'legacy config.json while nothing has moved');
      fs.mkdirSync(p.configDir, { recursive: true });
      fs.writeFileSync(p.configFile, JSON.stringify({ wikiDir: fwd(path.join(tmp, 'new-wiki')) }));
      assert.equal(path.resolve(init()), path.join(tmp, 'new-wiki'), 'the standard config.json wins');
    } finally {
      await fsp.rm(tmp, { recursive: true, force: true });
    }
  });
});

/** A pre-1.3 ~/.agent-wiki as install-local left it, and the standard folders it moves to. */
async function fixture() {
  const tmp = fs.realpathSync(await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-migrate-')));
  const legacy = path.join(tmp, 'home', '.agent-wiki');
  const local = path.join(tmp, 'home', 'AppData', 'Local', 'AgentWiki');
  const P = {
    config: path.join(tmp, 'home', 'AppData', 'Roaming', 'AgentWiki', 'config.json'),
    state: path.join(local, 'state', 'install-state.json'),
    logs: path.join(local, 'logs'),
    curatorDir: path.join(local, 'curator'),
    uiProfile: path.join(local, 'ui-profile'),
    runtime: path.join(local, 'runtime'),
    trayDir: path.join(local, 'tray'),
    serviceDir: path.join(local, 'service'),
    market: path.join(local, 'marketplace'),
    paste: path.join(local, 'paste-into-app-settings.md'),
  };
  const put = (rel, text) => {
    const f = path.join(legacy, rel);
    fs.mkdirSync(path.dirname(f), { recursive: true });
    fs.writeFileSync(f, text);
  };
  put('config.json', JSON.stringify({ wikiDir: 'C:/w', curator: { codexPath: 'C:/codex.exe', codexHome: fwd(path.join(legacy, 'curator', 'codex-home')) } }));
  put('install-state.json', JSON.stringify({ created: ['C:/elsewhere/AGENTS.md'], tray: { exe: path.join(legacy, 'tray', 'AgentWikiTray.exe') } }));
  put('logs/requests-2026-10-01.jsonl', '{"t":"1"}\n');
  put('logs/requests-2026-10-02.jsonl', '{"t":"2a"}\n');
  put('logs/http-sessions.json', '{"old":true}');
  put('curator/codex-home/auth.json', 'sign-in');
  put('curator/runs/r1.json', '{}');
  put('ui-profile/Local State', 'edge');
  put('runtime/server.mjs', 'old runtime');
  put('tray/AgentWikiTray.exe', 'tray');
  put('service/AgentWikiService.exe', 'svc');
  put('marketplace/plugins/agent-wiki/.mcp.json', '{}');
  put('paste-into-app-settings.md', 'pointer');
  return { tmp, legacy, P, items: migrationItems(P), journal: path.join(local, 'state', 'migration.json'), put };
}

describe('moving off ~/.agent-wiki', () => {
  test('plans, moves, merges logs, rewrites paths, resumes after a kill, removes the empty folder', async () => {
    const { tmp, legacy, P, items, journal } = await fixture();
    try {
      // The service already writes to the new logs folder, and a newer runtime is already in place.
      fs.mkdirSync(P.logs, { recursive: true });
      fs.writeFileSync(path.join(P.logs, 'requests-2026-10-02.jsonl'), '{"t":"2b"}\n');
      fs.writeFileSync(path.join(P.logs, 'http-sessions.json'), '{"new":true}');
      fs.mkdirSync(P.runtime, { recursive: true });
      fs.writeFileSync(path.join(P.runtime, 'server.mjs'), 'new runtime');

      const plan = await planMigration({ legacy, items });
      const action = Object.fromEntries(plan.items.map((i) => [i.name, i.action]));
      assert.equal(action.curator, 'move');
      assert.equal(action.logs, 'merge');
      assert.equal(action.runtime, 'replace');
      assert.deepEqual(plan.unknown, []);

      // Killed after the second move.
      let moves = 0;
      await assert.rejects(
        migrate({
          legacy,
          items,
          journal,
          log: (m) => {
            if (m.startsWith('moved') && ++moves === 2) throw new Error('killed');
          },
        }),
        /killed/,
      );
      assert.ok(fs.existsSync(legacy), 'interrupted: the old folder is still there');
      const half = JSON.parse(fs.readFileSync(journal, 'utf8'));
      assert.equal(half.items['config.json'].status, 'moved');

      const res = await migrate({ legacy, items, journal });
      assert.deepEqual(res.conflicts, []);
      assert.equal(res.removedLegacy, true);
      assert.ok(!fs.existsSync(legacy), 'the old folder is gone');

      assert.equal(fs.readFileSync(path.join(P.curatorDir, 'codex-home', 'auth.json'), 'utf8'), 'sign-in', 'the sign-in moved intact');
      assert.equal(fs.readFileSync(path.join(P.uiProfile, 'Local State'), 'utf8'), 'edge');
      assert.equal(fs.readFileSync(path.join(P.runtime, 'server.mjs'), 'utf8'), 'new runtime', 'the newer runtime is kept');
      assert.equal(fs.readFileSync(path.join(P.logs, 'requests-2026-10-02.jsonl'), 'utf8'), '{"t":"2b"}\n{"t":"2a"}\n', 'appended in place');
      assert.equal(fs.readFileSync(path.join(P.logs, 'requests-2026-10-01.jsonl'), 'utf8'), '{"t":"1"}\n');
      assert.equal(fs.readFileSync(path.join(P.logs, 'http-sessions.json'), 'utf8'), '{"new":true}');

      const cfg = JSON.parse(fs.readFileSync(P.config, 'utf8'));
      assert.equal(cfg.curator.codexHome, fwd(path.join(P.curatorDir, 'codex-home')), 'codexHome points at the new place, still with forward slashes');
      assert.equal(cfg.curator.codexPath, 'C:/codex.exe');
      const st = JSON.parse(fs.readFileSync(P.state, 'utf8'));
      assert.equal(st.tray.exe, path.join(P.trayDir, 'AgentWikiTray.exe'));
      assert.deepEqual(st.created, ['C:/elsewhere/AGENTS.md'], 'paths elsewhere are left alone');

      const j = JSON.parse(fs.readFileSync(journal, 'utf8'));
      assert.equal(j.items.curator.status, 'moved');
      assert.equal(j.items.logs.status, 'merged');
      assert.equal(j.items.runtime.status, 'replaced');
      assert.ok(j.finishedAt);

      // A second run has nothing to do.
      assert.equal((await planMigration({ legacy, items })).needed, false);
      assert.equal((await migrate({ legacy, items, journal })).removedLegacy, true);
    } finally {
      await fsp.rm(tmp, { recursive: true, force: true });
    }
  });

  test('keeps what is not ours, and the sign-in when both sides have one', async () => {
    const { tmp, legacy, P, items, journal, put } = await fixture();
    try {
      put('notes.txt', 'mine');
      fs.mkdirSync(path.join(P.curatorDir, 'codex-home'), { recursive: true });
      fs.writeFileSync(path.join(P.curatorDir, 'codex-home', 'auth.json'), 'another sign-in');
      const res = await migrate({ legacy, items, journal });
      assert.equal(res.removedLegacy, false);
      assert.deepEqual(res.unknown, ['notes.txt']);
      assert.match(res.conflicts.join('\n'), /^curator: exists at both/m);
      assert.equal(fs.readFileSync(path.join(legacy, 'curator', 'codex-home', 'auth.json'), 'utf8'), 'sign-in', 'never overwritten or deleted');
      assert.equal(fs.readFileSync(path.join(P.curatorDir, 'codex-home', 'auth.json'), 'utf8'), 'another sign-in');
      assert.equal(fs.readFileSync(path.join(legacy, 'notes.txt'), 'utf8'), 'mine');
    } finally {
      await fsp.rm(tmp, { recursive: true, force: true });
    }
  });

  test('rollback moves everything back and restores the old paths', async () => {
    const { tmp, legacy, P, items, journal } = await fixture();
    try {
      await migrate({ legacy, items, journal });
      assert.ok(!fs.existsSync(legacy));
      await rollback({ legacy, items, journal });
      assert.equal(fs.readFileSync(path.join(legacy, 'curator', 'codex-home', 'auth.json'), 'utf8'), 'sign-in');
      const cfg = JSON.parse(fs.readFileSync(path.join(legacy, 'config.json'), 'utf8'));
      assert.equal(cfg.curator.codexHome, fwd(path.join(legacy, 'curator', 'codex-home')));
      assert.ok(!fs.existsSync(P.curatorDir));
      assert.equal(JSON.parse(fs.readFileSync(journal, 'utf8')).items.curator.status, 'rolled-back');
    } finally {
      await fsp.rm(tmp, { recursive: true, force: true });
    }
  });

  test('recovers the copies an app package captured (install-local run inside Claude desktop on 2026-10-02)', async () => {
    const tmp = fs.realpathSync(await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-pkg-')));
    try {
      const env = { APPDATA: path.join(tmp, 'Roaming'), LOCALAPPDATA: path.join(tmp, 'Local') };
      const a = appPaths({ env, platform: 'win32', home: tmp });
      const P = {
        dataDir: a.dataDir, configDir: a.configDir, stateDir: a.stateDir, config: a.configFile, state: a.installState,
        logs: a.logDir, curatorDir: a.curatorDir, uiProfile: a.uiProfile, runtime: a.runtimeDir, trayDir: a.trayDir,
        serviceDir: a.serviceDir, market: a.marketDir, paste: a.pasteFile,
      };
      const cache = path.join(env.LOCALAPPDATA, 'Packages', 'Claude_pzs8sxrjxfjjc', 'LocalCache');
      const put = (rel, text) => {
        fs.mkdirSync(path.dirname(path.join(cache, rel)), { recursive: true });
        fs.writeFileSync(path.join(cache, rel), text);
      };
      put('Local/AgentWiki/curator/codex-home/auth.json', 'sign-in');
      put('Local/AgentWiki/state/install-state.json', '{"created":["x"]}');
      put('Local/AgentWiki/state/migration.json', '{"items":{}}');
      put('Local/AgentWiki/logs/requests-2026-10-02.jsonl', '{"t":"1"}\n');
      put('Local/AgentWiki/runtime/server.mjs', 'runtime');
      put('Roaming/AgentWiki/config.json', '{"wikiDir":"C:/w"}');
      fs.mkdirSync(path.join(env.LOCALAPPDATA, 'Packages', 'Other_123', 'LocalCache', 'Local'), { recursive: true });

      const found = packagedCopies(P, { localAppData: env.LOCALAPPDATA, appData: env.APPDATA });
      assert.deepEqual(found.map((c) => [c.pkg, path.basename(path.dirname(c.legacy))]), [['Claude_pzs8sxrjxfjjc', 'Local'], ['Claude_pzs8sxrjxfjjc', 'Roaming']]);
      for (const c of found) {
        const res = await migrate({ legacy: c.legacy, items: c.items, journal: c.journal });
        assert.equal(res.removedLegacy, true, c.legacy);
      }
      assert.equal(fs.readFileSync(path.join(P.curatorDir, 'codex-home', 'auth.json'), 'utf8'), 'sign-in');
      assert.equal(fs.readFileSync(P.config, 'utf8'), '{"wikiDir":"C:/w"}');
      assert.equal(fs.readFileSync(P.state, 'utf8'), '{"created":["x"]}');
      assert.ok(fs.existsSync(path.join(P.stateDir, 'migration.json')), 'the earlier journal comes along');
      assert.equal(fs.readFileSync(path.join(P.logs, 'requests-2026-10-02.jsonl'), 'utf8'), '{"t":"1"}\n');
      assert.ok(!fs.existsSync(path.join(cache, 'Local', 'AgentWiki')) && !fs.existsSync(path.join(cache, 'Roaming', 'AgentWiki')));
      assert.deepEqual(packagedCopies(P, { localAppData: env.LOCALAPPDATA, appData: env.APPDATA }), []);
    } finally {
      await fsp.rm(tmp, { recursive: true, force: true });
    }
  });

  test('mergeDir leaves a file it cannot merge, and remapper only touches paths under the old items', async () => {
    const tmp = fs.realpathSync(await fsp.mkdtemp(path.join(os.tmpdir(), 'aw-merge-')));
    try {
      const a = path.join(tmp, 'a');
      const b = path.join(tmp, 'b');
      fs.mkdirSync(a);
      fs.mkdirSync(b);
      fs.writeFileSync(path.join(a, 'x.json'), '1');
      fs.writeFileSync(path.join(b, 'x.json'), '2');
      fs.writeFileSync(path.join(a, 'same.txt'), 's');
      fs.writeFileSync(path.join(b, 'same.txt'), 's');
      const done = await mergeDir(a, b);
      assert.ok(done.some((d) => /x\.json: CONFLICT/.test(d)));
      assert.ok(fs.existsSync(path.join(a, 'x.json')) && !fs.existsSync(path.join(a, 'same.txt')));
      const map = remapper('/h/.agent-wiki', [{ name: 'logs', to: '/s/logs' }]);
      assert.equal(map('/h/.agent-wiki/logs/a.log'), fwd(path.join('/s/logs', 'a.log')));
      assert.equal(map('/h/.agent-wiki-other/logs'), '/h/.agent-wiki-other/logs');
      assert.equal(map('relative/path'), 'relative/path');
    } finally {
      await fsp.rm(tmp, { recursive: true, force: true });
    }
  });
});
