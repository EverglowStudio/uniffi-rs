import http from 'node:http';
import { readFile, realpath } from 'node:fs/promises';
import { dirname, extname, resolve, sep } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { deadline, assert } from './contract.mjs';

const rootDir = await realpath(dirname(fileURLToPath(import.meta.url)));
const playwrightPath = process.env.UNIFFI_TEST_PLAYWRIGHT_MODULE;
const executablePath = process.env.UNIFFI_TEST_CHROME;
assert(playwrightPath && executablePath, 'UNIFFI_TEST_PLAYWRIGHT_MODULE and UNIFFI_TEST_CHROME are required');
const { chromium } = await import(pathToFileURL(playwrightPath).href);
const mime = { '.js': 'text/javascript', '.mjs': 'text/javascript', '.wasm': 'application/wasm', '.json': 'application/json' };
const server = http.createServer(async (request, response) => {
  try {
    const pathname = decodeURIComponent(new URL(request.url, 'http://127.0.0.1').pathname);
    response.setHeader('Cache-Control', 'no-store');
    if (pathname === '/') {
      response.setHeader('Content-Type', 'text/html');
      response.end('<!doctype html><meta charset="utf-8"><title>S01 real async fixture</title>');
      return;
    }
    const file = await realpath(resolve(rootDir, `.${pathname}`));
    if (!file.startsWith(rootDir + sep)) { response.writeHead(403); response.end(); return; }
    response.setHeader('Content-Type', mime[extname(file)] ?? 'application/octet-stream');
    response.end(await readFile(file));
  } catch (error) { response.writeHead(404); response.end(String(error)); }
});
let browser;
let context;
const errors = [];
const watchdog = setTimeout(() => {
  console.error('deadline: Chrome fixture including teardown (150s)');
  process.exit(1);
}, 150000);
try {
  await deadline(new Promise((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', resolve);
  }), 'HTTP listen');
  // launch() creates an isolated temporary profile; never attaches to user's Chrome.
  browser = await chromium.launch({ executablePath, headless: true, timeout: 15000 });
  context = await browser.newContext();
  const page = await context.newPage();
  page.on('pageerror', (error) => errors.push(error.stack ?? String(error)));
  page.on('console', (message) => console.log(`[chrome] ${message.text()}`));
  await page.goto(`http://127.0.0.1:${server.address().port}/`, { timeout: 10000 });
  const windowResult = await deadline(page.evaluate(async () => {
    if (!(globalThis instanceof Window) || location.protocol !== 'http:') throw new Error('expected real HTTP Window');
    const { deadline, runContract } = await import('/contract.mjs');
    const root = await import('/generated/browser/index.js');
    return deadline(runContract(root, 'chrome-window'), 'Window suite', 55000);
  }), 'Chrome Window evaluation', 60000);
  console.log(JSON.stringify(windowResult));
  const workerResult = await deadline(page.evaluate(async () => {
    const worker = new Worker('/worker.mjs', { type: 'module' });
    let timer;
    try {
      return await new Promise((resolve, reject) => {
        timer = setTimeout(() => reject(new Error('deadline: DedicatedWorker suite (60s)')), 60000);
        worker.onmessage = ({ data }) => data.ok ? resolve(data.result) : reject(new Error(data.error));
        worker.onerror = (event) => reject(new Error(`Worker error: ${event.message}`));
        worker.onmessageerror = () => reject(new Error('Worker message decoding failed'));
      });
    } finally { clearTimeout(timer); worker.terminate(); }
  }), 'Chrome DedicatedWorker evaluation', 65000);
  console.log(JSON.stringify(workerResult));
  assert(errors.length === 0, `Chrome page errors: ${errors.join('\n')}`);
} finally {
  try {
    if (context) await deadline(context.close(), 'close browser context', 5000);
  } finally {
    try {
      if (browser) await deadline(browser.close(), 'close Chrome', 5000);
    } finally {
      server.closeAllConnections();
      await deadline(new Promise((resolve) => server.close(resolve)), 'close HTTP server', 5000);
      clearTimeout(watchdog);
    }
  }
}
