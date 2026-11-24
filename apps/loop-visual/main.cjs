'use strict';
const { app, BrowserWindow, desktopCapturer, screen, systemPreferences, ipcMain, session } = require('electron');
const { spawn } = require('node:child_process');
const path = require('node:path');
const fs = require('node:fs');
const runtimeScript = fs.readFileSync(path.join(__dirname, 'three-bundle.js'), 'utf8');
const { pathToFileURL } = require('node:url');
const { createCapture } = require('./capture.cjs');
const { safeState, LineDecoder } = require('./protocol.cjs');

let panel, visual, sidecar, capture, visualLoaded;
let quitting = false, ready = false, requestCounter = 0, activeId = null;
let sceneCounter = 0, currentScene = null, modelLabel = '', lastError = null, lastStatus = null;
const assets = ['index.html', 'renderer.js', 'style.css', 'scene.html', 'scene.js'];
const allowedFiles = new Set(assets.map((name) => pathToFileURL(path.join(__dirname, name)).href));
app.setName('Loop Visual');

function notify(event) {
  if (event.type === 'status') lastStatus = event;
  if (event.type === 'error') lastError = event;
  if (panel && !panel.isDestroyed()) panel.webContents.send('visual:event', event);
}
function write(command) {
  if (!sidecar || sidecar.killed || !sidecar.stdin.writable) throw new Error('Loop sidecar is unavailable. Restart the app.');
  sidecar.stdin.write(`${JSON.stringify(command)}\n`);
}
function authorized(event, window) {
  return window && !window.isDestroyed() && event.sender === window.webContents
    && event.senderFrame === window.webContents.mainFrame;
}
function protect(window) {
  window.webContents.setWindowOpenHandler(() => ({ action: 'deny' }));
  window.webContents.on('will-navigate', (event) => event.preventDefault());
  window.webContents.on('will-frame-navigate', (event, details) => {
    const url = event.url || details?.url || (typeof details === 'string' ? details : '');
    if (!['about:blank', 'about:srcdoc'].includes(url)) event.preventDefault();
  });
  window.webContents.on('will-attach-webview', (event) => event.preventDefault());
}
function cancel() {
  const id = activeId;
  activeId = null;
  requestCounter++;
  if (id !== null) { try { write({ type: 'cancel', requestId: id }); } catch {} }
  notify({ type: 'cancelled' });
}
function closeScene() {
  cancel();
  const target = visual;
  currentScene = null;
  visual = null;
  if (target && !target.isDestroyed()) target.destroy();
}
async function showScene(event) {
  const sceneId = ++sceneCounter;
  const nextScene = { sceneId, html: event.html, title: event.title, state: {}, runtimeScript };
  if (!visual || visual.isDestroyed()) {
    const area = screen.getPrimaryDisplay().workArea;
    visual = new BrowserWindow({ width: 860, height: 680, minWidth: 420, minHeight: 320,
      x: area.x + Math.max(0, area.width - Math.min(860, area.width)), y: area.y + 24,
      title: event.title, alwaysOnTop: true, show: false, backgroundColor: '#faf9f5',
      webPreferences: { preload: path.join(__dirname, 'scene-preload.cjs'), sandbox: true,
        contextIsolation: true, nodeIntegration: false, webviewTag: false, spellcheck: false },
    });
    const created = visual;
    protect(created);
    created.on('closed', () => {
      if (visual !== created) return;
      visual = null; currentScene = null;
      if (!quitting) cancel();
    });
    visualLoaded = created.loadFile(path.join(__dirname, 'scene.html'));
  }
  const target = visual;
  await visualLoaded;
  if (requestCounter !== event.requestId || sceneCounter !== sceneId || visual !== target || target.isDestroyed()) return;
  currentScene = nextScene;
  target.webContents.send('scene:render', currentScene);
  target.setTitle(event.title);
  target.show();
  target.moveTop();
  target.focus();
  notify({ type: 'visual', requestId: event.requestId, title: event.title });
}
function handleEvent(event) {
  if (!event || typeof event !== 'object') return;
  if (event.type === 'ready') {
    ready = true; modelLabel = event.model; lastError = null; notify(event); return;
  }
  if (event.requestId !== undefined && event.requestId !== activeId) return;
  if (event.type === 'visual') {
    if (typeof event.html !== 'string' || event.html.length > 512 * 1024 || typeof event.title !== 'string') return;
    void showScene(event).catch((error) => {
      if (requestCounter !== event.requestId) return;
      if (activeId === event.requestId) {
        activeId = null;
        try { write({ type: 'cancel', requestId: event.requestId }); } catch {}
      }
      notify({ type: 'error', requestId: event.requestId, message: error.message });
    });
  } else if (event.type === 'design') {
    notify({type:'status',state:'working',message:'Visual design ready; rendering with GPT-6.1 Sol…'});
  } else if (event.type === 'progress') {
    notify({ type: 'status', state: 'working', message: event.message });
  } else {
    notify(event);
    if (event.type === 'done' || event.type === 'error') activeId = null;
  }
}
function startSidecar() {
  const binary = process.env.LOOP_VISUAL_BIN || path.join(__dirname, '../../target/debug/loop-visual');
  sidecar = spawn(binary, [], { cwd: path.resolve(__dirname, '../..'), stdio: ['pipe', 'pipe', 'pipe'] });
  const decoder = new LineDecoder(handleEvent, (error) => notify({ type: 'error', message: error.message }));
  sidecar.stdout.setEncoding('utf8');
  sidecar.stdout.on('data', (chunk) => decoder.push(chunk));
  sidecar.stderr.on('data', (chunk) => process.stderr.write(chunk));
  sidecar.stdin.on('error', () => {});
  sidecar.on('error', (error) => { ready = false; activeId = null; notify({ type: 'error', message: `Could not start Loop: ${error.message}. Run npm run build:sidecar.` }); });
  sidecar.on('exit', (code) => { ready = false; activeId = null; if (!quitting && !lastError) notify({ type: 'error', message: `Loop sidecar exited (${code}). Restart after checking provider configuration.` }); });
}

ipcMain.on('visual:listen', (event) => {
  if (!authorized(event, panel)) return;
  if (ready) notify({ type: 'ready', model: modelLabel });
  if (lastStatus) notify(lastStatus);
  if (lastError) notify(lastError);
});
ipcMain.handle('visual:prompt', async (event, text) => {
  if (!authorized(event, panel)) throw new Error('Invalid sender');
  if (!ready) throw new Error(lastError?.message || 'Loop is still starting.');
  if (activeId !== null) throw new Error('Stop the current request first.');
  if (typeof text !== 'string' || !text.trim() || text.length > 16000) throw new Error('Enter a question under 16,000 characters.');
  const id = ++requestCounter;
  activeId = id; lastError = null;
  notify({ type: 'started', requestId: id });
  try {
    const frame = await capture.refresh();
    if (activeId !== id || quitting) return;
    write({ type: 'prompt', requestId: id, text,
      image: { data: frame.data, mimeType: frame.mimeType },
      context: { screen: { width: frame.width, height: frame.height, logicalWidth: frame.logicalWidth,
        logicalHeight: frame.logicalHeight, scaleFactor: frame.scaleFactor, capturedAt: frame.capturedAt,
        description: frame.context }, visual: currentScene ? { title: currentScene.title, html: currentScene.html, state: currentScene.state } : null },
    });
  } catch (error) {
    if (activeId !== id) return;
    activeId = null; notify({ type: 'error', requestId: id, message: error.message });
  }
});
ipcMain.handle('visual:cancel', (event) => { if (authorized(event, panel)) cancel(); });
ipcMain.handle('visual:close', (event) => { if (authorized(event, panel)) closeScene(); });
ipcMain.handle('visual:permissions', async (event) => {
  if (authorized(event, panel)) return capture.setupPermissions();
});
ipcMain.on('scene:ready', (event) => {
  if (authorized(event, visual) && currentScene) visual.webContents.send('scene:render', currentScene);
});
ipcMain.on('scene:state', (event, value) => {
  if (!authorized(event, visual) || !currentScene || value?.sceneId !== currentScene.sceneId) return;
  const state = safeState(value.state);
  if (state) currentScene.state = state;
});

app.whenReady().then(async () => {
  session.defaultSession.setPermissionRequestHandler((_, __, callback) => callback(false));
  session.defaultSession.setPermissionCheckHandler(() => false);
  session.defaultSession.webRequest.onBeforeRequest((details, callback) => {
    callback({ cancel: !(allowedFiles.has(details.url) || details.url.startsWith('data:') || details.url.startsWith('about:')) });
  });
  panel = new BrowserWindow({ width: 440, height: 580, minWidth: 380, minHeight: 480,
    x: screen.getPrimaryDisplay().workArea.x + 12, y: screen.getPrimaryDisplay().workArea.y + 24,
    title: 'Loop Visual', alwaysOnTop: true, backgroundColor: '#f7f5ef',
    webPreferences: { preload: path.join(__dirname, 'preload.cjs'), sandbox: true,
      contextIsolation: true, nodeIntegration: false, webviewTag: false, spellcheck: false },
  });
  protect(panel);
  panel.on('closed', () => app.quit());
  capture = createCapture({ desktopCapturer, screen, systemPreferences,
    windows: () => [panel, visual].filter(Boolean), onStatus: (status) => notify({ type: 'status', ...status }),
  });
  await panel.loadFile(path.join(__dirname, 'index.html'));
  startSidecar();
  capture.start();
});
app.on('before-quit', () => {
  if (quitting) return;
  quitting = true; activeId = null; currentScene = null;
  capture?.stop();
  try { write({ type: 'shutdown' }); } catch {}
  if (sidecar && !sidecar.killed) {
    const child = sidecar;
    setTimeout(() => child.kill('SIGKILL'), 3500).unref();
    child.stdin.end();
  }
});
app.on('window-all-closed', () => app.quit());
