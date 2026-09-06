import { deadline, runContract } from './contract.mjs';

const backend = process.argv[2];
if (!['napi', 'wasm'].includes(backend)) throw new Error('expected napi or wasm');
// A hard deadline also bounds imports and native calls that never wake JS.
const watchdog = setTimeout(() => {
  console.error(`deadline: Node ${backend} fixture (60s)`);
  process.exit(1);
}, 60000);
try {
  const root = await import(backend === 'napi' ? './generated/node/index.js' : './generated/browser/index.js');
  console.log(JSON.stringify(await deadline(runContract(root, `node-${backend}`), backend, 55000)));
} finally { clearTimeout(watchdog); }
