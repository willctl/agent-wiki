// npm run uninstall-service   (run in an ELEVATED terminal: "Run as administrator")
//
// Stops and deletes the AgentWiki service and removes the folder grants for
// NT SERVICE\AgentWiki. Never touches the wiki content. Afterwards run
// `npm run install-local` (normal terminal) to switch the apps back to stdio.

import fs from 'node:fs';
import path from 'node:path';
import { InstallError, P, REPO, SERVICE_ACCOUNT, SERVICE_NAME, fwd, info, isElevated, readJson, run, serviceState, step, warn } from './lib.mjs';

async function main() {
  if (process.platform !== 'win32') throw new InstallError('The service is Windows-only (there is nothing to uninstall here).');
  if (!isElevated()) {
    throw new InstallError(`This needs administrator rights. In a terminal opened with "Run as administrator":\n  cd "${REPO}"\n  npm run uninstall-service`);
  }
  const cfg = await readJson(P.config).catch(() => null);
  step(1, 'Stop and delete the service');
  const state = serviceState();
  if (state) {
    if (state !== 'STOPPED') run('sc.exe', ['stop', SERVICE_NAME]);
    for (let i = 0; i < 40 && serviceState() !== 'STOPPED'; i++) await new Promise((r) => setTimeout(r, 500));
    const d = run('sc.exe', ['delete', SERVICE_NAME]);
    info(d.ok ? 'service deleted' : `sc delete: ${(d.stdout + d.stderr).trim()}`);
  } else info('service not installed');

  step(2, 'Remove folder grants');
  for (const target of [cfg?.wikiDir && path.resolve(cfg.wikiDir), P.configDir, P.dataDir, P.logs, P.legacyHome].filter((t) => t && fs.existsSync(t))) {
    const r = run('icacls.exe', [target, '/remove:g', SERVICE_ACCOUNT, '/Q']);
    if (r.ok) info(`removed ${SERVICE_ACCOUNT} from ${fwd(target)}`);
    else warn(`icacls ${fwd(target)}: ${(r.stdout + r.stderr).trim()}`);
  }

  console.log(`\nService removed. The wiki is untouched at ${cfg?.wikiDir ?? fwd(P.defaultWiki)}.`);
  console.log(`Now run \`npm run install-local\` in a normal terminal (in ${REPO}) to switch the apps back to stdio.`);
}

main().catch((e) => {
  console.error(`\nService uninstall failed: ${e instanceof InstallError ? e.message : e?.stack || e}`);
  process.exit(1);
});
