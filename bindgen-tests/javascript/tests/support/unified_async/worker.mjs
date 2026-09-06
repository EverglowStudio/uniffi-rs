import { deadline, runContract } from './contract.mjs';

try {
  if (!(globalThis instanceof DedicatedWorkerGlobalScope)) throw new Error('expected a real DedicatedWorker');
  const root = await import('./generated/browser/index.js');
  self.postMessage({ ok: true, result: await deadline(runContract(root, 'chrome-worker'), 'Worker suite', 55000) });
} catch (error) {
  self.postMessage({ ok: false, error: error?.stack ?? String(error) });
} finally { self.close(); }
