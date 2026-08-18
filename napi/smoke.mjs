// Standalone smoke test for the libradicle napi addon — the future Freedom
// Browser desktop path (no radicle-httpd, no localhost API).
//
// Usage: node smoke.mjs <homeDir> <rid> [addonPath]
//
// NOTE: the public seeds allow one connection per IP; stop any local
// radicle-node daemon before running (`rad node stop`).

import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const [home, rid, addonPath] = process.argv.slice(2);
if (!home || !rid) {
  console.error('usage: node smoke.mjs <homeDir> <rid> [addonPath]');
  process.exit(2);
}

const addon = require(addonPath ?? '../target/debug/libradicle.node');
const check = (label, raw) => {
  const v = JSON.parse(raw);
  if (v.error) {
    console.error(`${label}: ${v.error}`);
    process.exit(1);
  }
  console.log(label, v);
  return v;
};

check('start:', await addon.start(home, 'libradicle-napi-smoke'));
check('connectSeeds:', await addon.connectSeeds(15000));
check('cloneRepo:', await addon.cloneRepo(rid, 120000));
check('repoInfo:', await addon.repoInfo(rid));
const files = check('listFiles:', await addon.listFiles(rid));
console.log(`files at head: ${files.length}`);
check('shutdown:', await addon.shutdown());
console.log('OK');
