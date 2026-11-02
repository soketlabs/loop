'use strict';

const { app, BrowserWindow, ipcMain } = require('electron');
const { createServer } = require('node:http');
const path = require('node:path');
const fs = require('node:fs');

const APP_DIR = __dirname;
const SCENE_ID = 7301;
const TIMEOUT_MS = 7000;
const fixture = (probeUrl) => `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Renderer isolation fixture</title></head>
<body>
  <div id="stage" style="height:200px"></div><label for="level">Level</label>
  <input id="level" type="range" min="0" max="100" value="20">
  <output id="value">20</output>
  <script>
    const world = createWorld(document.querySelector('#stage'));
    world.scene.add(new THREE.Mesh(new THREE.SphereGeometry(1), new THREE.MeshStandardMaterial({color:0x22ddbb})));
    const input = document.querySelector('#level');
    const output = document.querySelector('#value');
    const checks = {
      opaqueOrigin: location.origin === 'null',
      webgl: world.renderer.domElement instanceof HTMLCanvasElement && world.renderer.getContext() !== null,
      noNode: typeof require === 'undefined' && typeof process === 'undefined',
      noLoopScene: typeof window.loopScene === 'undefined',
      noLoopVisual: typeof window.loopVisual === 'undefined',
      parentDomBlocked: (() => { try { void parent.document.body; return false; } catch { return true; } })(),
    };
    const report = (stage, value = Number(input.value)) => window.visualState({
      selected: 'renderer-smoke:' + stage,
      controls: { fixture: 'loop-renderer-smoke', stage, value, ...checks },
    });
    input.addEventListener('input', () => {
      output.value = input.value;
      output.textContent = input.value;
      report('slider');
    });
    window.addEventListener('message', (event) => {
      if (event.data && event.data.kind === 'loop-renderer-smoke-trigger') {
        input.value = String(event.data.value);
        input.dispatchEvent(new Event('input', { bubbles: true }));
      }
    });
    report('loaded');
    fetch(${JSON.stringify(probeUrl)}).then(() => {
      report('network-allowed');
    }).catch(() => {
      report('network-blocked');
    });
  </script>
</body></html>`;

let windowRef;
let probeServer;
let probeHits = 0;
let finished = false;
let timer;
let resolveDone;
let rejectDone;
const done = new Promise((resolve, reject) => { resolveDone = resolve; rejectDone = reject; });
const states = [];
let triggerSent = false;

function finish(error) {
  if (finished) return;
  finished = true;
  clearTimeout(timer);
  if (error) rejectDone(error);
  else resolveDone();
}

function fail(message) {
  finish(new Error(message));
}

function maybeFinish() {
  const loaded = states.find((item) => item.controls?.stage === 'loaded');
  const slider = states.find((item) => item.controls?.stage === 'slider');
  const networkBlocked = states.find((item) => item.controls?.stage === 'network-blocked');
  if (!loaded || !slider || !networkBlocked) return;
  for (const state of states) {
    const allowedControlKeys = ['fixture', 'stage', 'value', 'opaqueOrigin', 'noNode', 'noLoopScene', 'noLoopVisual', 'parentDomBlocked', 'webgl'];
    if (state.selected !== `renderer-smoke:${state.controls?.stage}`
      || state.controls?.fixture !== 'loop-renderer-smoke'
      || !['loaded', 'slider', 'network-blocked'].includes(state.controls.stage)
      || Object.keys(state).sort().join(',') !== 'controls,selected'
      || Object.keys(state.controls).sort().join(',') !== allowedControlKeys.sort().join(',')) {
      return fail(`Unexpected state payload: ${JSON.stringify(state)}`);
    }
  }
  const checks = loaded.controls;
  for (const [name, value] of Object.entries({
    webgl: true, opaqueOrigin: true, noNode: true, noLoopScene: true, noLoopVisual: true, parentDomBlocked: true,
  })) {
    if (checks?.[name] !== value) return fail(`Isolation check ${name} failed: ${JSON.stringify(checks)}`);
  }
  if (slider.controls.value !== 73) return fail(`Slider did not update locally: ${JSON.stringify(slider)}`);
  if (networkBlocked.controls.opaqueOrigin !== true) return fail('Network result came from a frame without opaque origin');
  if (probeHits !== 0) return fail(`connect-src allowed a fetch (${probeHits} local request(s))`);
  finish();
}

async function run() {
  probeServer = createServer((_request, response) => {
    probeHits += 1;
    response.writeHead(200, { 'content-type': 'text/plain' }).end('unexpected network access');
  });
  probeServer.listen(0, '127.0.0.1');
  await new Promise((resolve, reject) => {
    probeServer.once('listening', resolve);
    probeServer.once('error', reject);
  });
  const probeUrl = `http://127.0.0.1:${probeServer.address().port}/probe`;

  ipcMain.on('scene:ready', (event) => {
    if (!windowRef || event.sender !== windowRef.webContents) return fail('Unexpected scene:ready sender');
    windowRef.webContents.send('scene:render', {
      sceneId: SCENE_ID,
      runtimeScript: fs.readFileSync(path.join(APP_DIR, 'three-bundle.js'), 'utf8'),
      title: 'Renderer smoke fixture',
      html: fixture(probeUrl),
      state: {},
    });
  });
  ipcMain.on('scene:state', (event, payload) => {
    if (!windowRef || event.sender !== windowRef.webContents) return fail('Unexpected scene:state sender');
    if (!payload || payload.sceneId !== SCENE_ID || !payload.state || typeof payload.state !== 'object') {
      return fail(`Unexpected scene state envelope: ${JSON.stringify(payload)}`);
    }
    states.push(payload.state);
    if (payload.state.controls?.stage === 'loaded' && !triggerSent) {
      triggerSent = true;
      windowRef.webContents.executeJavaScript(`(() => {
        const frame = document.querySelector('#visual');
        frame.contentWindow.postMessage({ kind: 'loop-renderer-smoke-trigger', value: 73 }, '*');
      })()`).catch((error) => fail(`Could not trigger the fixture slider: ${error.message}`));
    }
    maybeFinish();
  });

  windowRef = new BrowserWindow({
    width: 520,
    height: 420,
    show: false,
    webPreferences: {
      preload: path.join(APP_DIR, 'scene-preload.cjs'),
      sandbox: true,
      contextIsolation: true,
      nodeIntegration: false,
      webviewTag: false,
    },
  });
  windowRef.webContents.on('console-message', (_event, level, message) => {
    if (level >= 2) process.stderr.write(`[renderer console] ${message}\n`);
  });
  windowRef.webContents.on('render-process-gone', (_event, details) => fail(`Renderer exited: ${details.reason}`));
  windowRef.on('closed', () => { windowRef = null; });
  timer = setTimeout(() => fail(`Renderer smoke timed out; states=${JSON.stringify(states)}`), TIMEOUT_MS);
  timer.unref();
  await windowRef.loadFile(path.join(APP_DIR, 'scene.html'));
  await done;
}

app.whenReady().then(run).then(() => {
  process.stdout.write('renderer smoke passed: srcdoc scripts, slider state bridge, isolation, CSP network block\n');
  app.quit();
}, (error) => {
  process.stderr.write(`renderer smoke failed: ${error.stack || error.message}\n`);
  process.exitCode = 1;
  app.quit();
});

app.on('before-quit', () => {
  if (windowRef && !windowRef.isDestroyed()) windowRef.destroy();
  if (probeServer?.listening) probeServer.closeAllConnections();
  probeServer?.close();
});
