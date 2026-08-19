// Progress + cancellation smoke for the napi addon.
//   node progress-smoke.mjs <homeDir> <rid> [--cancel]
// Stop any system radicle-node first (one connection per IP on the seeds).

import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const addon = require('../target/debug/libradicle.node');

const [home, rid, flag] = process.argv.slice(2);
if (!home || !rid) {
  console.error('usage: node progress-smoke.mjs <homeDir> <rid> [--cancel]');
  process.exit(2);
}
const doCancel = flag === '--cancel';

const j = (raw) => JSON.parse(raw);

const started = j(await addon.start(home, 'progress-smoke'));
if (started.error) throw new Error(started.error);
console.log('node started:', started.did);
console.log('connect:', await addon.connectSeeds(15000));

let events = 0;
const clonePromise = addon.cloneRepoWithProgress(rid, 120000, (event) => {
  events++;
  const e = j(event);
  console.log(`  [progress] ${event}`);
  // Cancel as soon as we see the first fetch begin.
  if (doCancel && e.phase === 'fetching') {
    console.log('  >>> requesting cancel');
    console.log('  cancelClone:', addon.cancelClone(rid));
  }
});

const result = j(await clonePromise);
console.log('clone result:', result, `(${events} progress events)`);
console.log(await addon.shutdown());
process.exit(result.ok || result.cancelled ? 0 : 1);
