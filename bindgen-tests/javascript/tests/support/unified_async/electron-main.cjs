const { app, BrowserWindow } = require('electron');
const { join } = require('node:path');

app.whenReady().then(async () => {
  const window = new BrowserWindow({
    show: false,
    webPreferences: {
      contextIsolation: true,
      nodeIntegration: false,
      sandbox: false,
      preload: join(__dirname, 'generated/electron/preload.cjs'),
    },
  });
  await window.loadFile(join(__dirname, 'electron.html'));
});
app.on('window-all-closed', () => app.quit());
