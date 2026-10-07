// Where Agent Wiki keeps its own files (not the wiki): the platform's standard
// locations, never a dot folder in the home directory.
//
//                Windows                        Linux (XDG)                       macOS
//   config       %APPDATA%\AgentWiki            $XDG_CONFIG_HOME/agent-wiki       ~/Library/Application Support/Agent Wiki
//   data         %LOCALAPPDATA%\AgentWiki       $XDG_DATA_HOME/agent-wiki         ~/Library/Application Support/Agent Wiki
//   state        <data>\state                   $XDG_STATE_HOME/agent-wiki        <data>/state
//   logs         <data>\logs                    <state>/logs                      ~/Library/Logs/Agent Wiki
//   cache        <data>                         $XDG_CACHE_HOME/agent-wiki        ~/Library/Caches/Agent Wiki
//
// On macOS an XDG_* variable that is set wins for its kind. Overrides, strongest first:
// AGENT_WIKI_{CONFIG,DATA,STATE,LOG,CACHE}_DIR for one kind (the service passes these, since it
// runs as another account with another profile), then AGENT_WIKI_HOME for "everything in this one
// folder" (portable mode: the tests and ui:preview; the layout of the old ~/.agent-wiki).

import os from 'node:os';
import path from 'node:path';

/** The folder every version before 1.3 used, which `install-local` moves away from. */
export const legacyHome = (home = os.homedir()) => path.join(home, '.agent-wiki');

const OVERRIDES = { configDir: 'AGENT_WIKI_CONFIG_DIR', dataDir: 'AGENT_WIKI_DATA_DIR', stateDir: 'AGENT_WIKI_STATE_DIR', logDir: 'AGENT_WIKI_LOG_DIR', cacheDir: 'AGENT_WIKI_CACHE_DIR' };

// The XDG spec says to ignore a relative path in these variables.
const xdg = (env, name, fallback) => (env[name] && path.isAbsolute(env[name]) ? env[name] : fallback);

function standardDirs(env, platform, home) {
  if (platform === 'win32') {
    const roaming = env.APPDATA || path.join(home, 'AppData', 'Roaming');
    const local = env.LOCALAPPDATA || path.join(home, 'AppData', 'Local');
    const data = path.join(local, 'AgentWiki');
    return { configDir: path.join(roaming, 'AgentWiki'), dataDir: data, stateDir: path.join(data, 'state'), logDir: path.join(data, 'logs'), cacheDir: data };
  }
  if (platform === 'darwin') {
    const support = path.join(home, 'Library', 'Application Support', 'Agent Wiki');
    const state = env.XDG_STATE_HOME && path.isAbsolute(env.XDG_STATE_HOME) ? path.join(env.XDG_STATE_HOME, 'agent-wiki') : path.join(support, 'state');
    return {
      configDir: env.XDG_CONFIG_HOME && path.isAbsolute(env.XDG_CONFIG_HOME) ? path.join(env.XDG_CONFIG_HOME, 'agent-wiki') : support,
      dataDir: env.XDG_DATA_HOME && path.isAbsolute(env.XDG_DATA_HOME) ? path.join(env.XDG_DATA_HOME, 'agent-wiki') : support,
      stateDir: state,
      logDir: env.XDG_STATE_HOME && path.isAbsolute(env.XDG_STATE_HOME) ? path.join(state, 'logs') : path.join(home, 'Library', 'Logs', 'Agent Wiki'),
      cacheDir: env.XDG_CACHE_HOME && path.isAbsolute(env.XDG_CACHE_HOME) ? path.join(env.XDG_CACHE_HOME, 'agent-wiki') : path.join(home, 'Library', 'Caches', 'Agent Wiki'),
    };
  }
  const state = path.join(xdg(env, 'XDG_STATE_HOME', path.join(home, '.local', 'state')), 'agent-wiki');
  return {
    configDir: path.join(xdg(env, 'XDG_CONFIG_HOME', path.join(home, '.config')), 'agent-wiki'),
    dataDir: path.join(xdg(env, 'XDG_DATA_HOME', path.join(home, '.local', 'share')), 'agent-wiki'),
    stateDir: state,
    logDir: path.join(state, 'logs'),
    cacheDir: path.join(xdg(env, 'XDG_CACHE_HOME', path.join(home, '.cache')), 'agent-wiki'),
  };
}

/**
 * Every location Agent Wiki uses besides the wiki itself. `mode` is "portable" when
 * AGENT_WIKI_HOME puts everything in one folder, otherwise "standard".
 */
export function appPaths({ env = process.env, platform = process.platform, home = os.homedir() } = {}) {
  const portable = env.AGENT_WIKI_HOME ? path.resolve(env.AGENT_WIKI_HOME) : null;
  const dirs = portable
    ? { configDir: portable, dataDir: portable, stateDir: portable, logDir: path.join(portable, 'logs'), cacheDir: portable }
    : standardDirs(env, platform, home);
  for (const [k, name] of Object.entries(OVERRIDES)) if (env[name]) dirs[k] = path.resolve(env[name]);
  return {
    mode: portable ? 'portable' : 'standard',
    ...dirs,
    configFile: path.join(dirs.configDir, 'config.json'),
    installState: path.join(dirs.stateDir, 'install-state.json'),
    migrationJournal: path.join(dirs.stateDir, 'migration.json'),
    // In logs/, not state/: the service may write logs/ and nothing else of ours.
    httpSessions: path.join(dirs.logDir, 'http-sessions.json'),
    runtimeDir: path.join(dirs.dataDir, 'runtime'),
    serviceDir: path.join(dirs.dataDir, 'service'),
    trayDir: path.join(dirs.dataDir, 'tray'),
    marketDir: path.join(dirs.dataDir, 'marketplace'),
    curatorDir: path.join(dirs.dataDir, 'curator'),
    curatorCodexHome: path.join(dirs.dataDir, 'curator', 'codex-home'),
    pasteFile: path.join(dirs.dataDir, 'paste-into-app-settings.md'),
    uiProfile: path.join(dirs.cacheDir, 'ui-profile'),
    legacyHome: legacyHome(home),
  };
}

/** Environment that makes a child process (another server, the read-only Ask server) use the same locations. */
export function pathsEnv(p = appPaths()) {
  return Object.fromEntries(Object.entries(OVERRIDES).map(([k, name]) => [name, p[k]]));
}
