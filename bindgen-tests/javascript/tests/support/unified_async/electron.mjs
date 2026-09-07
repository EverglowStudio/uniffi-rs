import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { deadline, assert } from './contract.mjs';

const directory = dirname(fileURLToPath(import.meta.url));
const executablePath = process.env.UNIFFI_TEST_ELECTRON;
assert(executablePath && process.env.UNIFFI_TEST_PLAYWRIGHT_MODULE, 'Electron and Playwright paths are required');
const { _electron } = await import(pathToFileURL(process.env.UNIFFI_TEST_PLAYWRIGHT_MODULE).href);
const watchdog = setTimeout(() => { console.error('deadline: Electron fixture'); process.exit(1); }, 80000);
let app;
try {
  app = await _electron.launch({ executablePath, args: [join(directory, 'electron-main.cjs')], timeout: 15000 });
  console.log(JSON.stringify(await app.evaluate(({ BrowserWindow }) => {
    const preferences = BrowserWindow.getAllWindows()[0].webContents.getLastWebPreferences();
    return { electron: process.versions.electron, node: process.versions.node, chrome: process.versions.chrome, contextIsolation: preferences.contextIsolation, nodeIntegration: preferences.nodeIntegration };
  })));
  const window = await app.firstWindow();
  window.on('console', message => console.log(`[electron] ${message.text()}`));
  const errors = [];
  window.on('pageerror', error => errors.push(error.stack ?? String(error)));
  const result = await deadline(window.evaluate(async (directory) => {
    if (!(globalThis instanceof Window) || typeof globalThis.require !== 'undefined' || !window.__uniffiBackend) throw new Error('expected isolated Electron renderer with generated preload');
    const { runContract } = await import(`${directory}/contract.mjs`);
    const root = await import(`${directory}/generated/electron/index.js`);
    return runContract(root, 'electron-renderer');
  }, pathToFileURL(directory).href), 'Electron renderer suite', 60000);
  assert(errors.length === 0, `Electron page errors: ${errors.join('\n')}`);
  console.log(JSON.stringify(result));
} finally {
  if (app) await deadline(app.close(), 'Electron app close', 5000);
  clearTimeout(watchdog);
}
