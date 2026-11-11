'use strict';

const { execFile } = require('node:child_process');
const { promisify } = require('node:util');
const execFileAsync = promisify(execFile);

const CAPTURE_INTERVAL_MS = 3000;
const CAPTURE_TIMEOUT_MS = 5000;
const LOOP_UI_CONTEXT_NOTE = 'Loop Visual panels may occlude desktop content.';

function timeoutAfter(promise, timeoutMs) {
  let timeout;
  return Promise.race([
    promise,
    new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error(`Capture timed out after ${timeoutMs} ms.`)), timeoutMs);
    }),
  ]).finally(() => clearTimeout(timeout));
}

function imageDimensions(data, mimeType) {
  const bytes = Buffer.from(data, 'base64');
  if (mimeType === 'image/png' && bytes.length >= 24 && bytes.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))) {
    return { width: bytes.readUInt32BE(16), height: bytes.readUInt32BE(20) };
  }
  if (mimeType === 'image/jpeg' && bytes.length > 4) {
    let offset = 2;
    while (offset + 9 < bytes.length) {
      if (bytes[offset] !== 0xff) { offset += 1; continue; }
      const marker = bytes[offset + 1];
      const segmentLength = bytes.readUInt16BE(offset + 2);
      if ([0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf].includes(marker)) {
        return { height: bytes.readUInt16BE(offset + 5), width: bytes.readUInt16BE(offset + 7) };
      }
      if (segmentLength < 2) break;
      offset += 2 + segmentLength;
    }
  }
  return null;
}

function boundedContext(result) {
  const text = typeof result.text === 'string' ? result.text.trim() : '';
  let structured = '';
  if (result.structuredJson != null) {
    try { structured = JSON.stringify(result.structuredJson); } catch { structured = ''; }
  }
  const pieces = [text, structured].filter(Boolean);
  return pieces.join('\n').slice(0, 1200) || 'Primary display';
}

function createCapture({ desktopCapturer, screen, windows = () => [], onStatus = () => {}, driverSDK, nativeImage,
  platform = process.platform, permissionGrant = () => execFileAsync('cua-driver', ['permissions', 'grant'], { timeout: 120000, maxBuffer: 1024 * 1024 }) }) {
  let latestFrame = null;
  let timer = null;
  let stopping = false;
  let refreshInFlight = null;
  let driver = null;
  let driverUnavailable = false;
  let driverDisabled = false;
  let driverFailure = null;
  let electronDisabled = false;
  let electronFailure = null;
  const retiredDrivers = [];

  async function getSdk() {
    if (driverSDK === false) return null;
    return driverSDK || await import('@trycua/cua-driver');
  }

  function status(state, message, backend = 'electron-desktopCapturer') {
    try { onStatus({ state, message, backend }); } catch { /* status observers must not break capture */ }
  }

  async function getDriver() {
    if (driver || driverUnavailable || driverDisabled) return driver;
    try {
      const sdk = await getSdk();
      if (!sdk) { driverUnavailable = true; return null; }
      driver = sdk.CuaDriver.connect(undefined);
      return driver;
    } catch {
      driverUnavailable = true;
      return null;
    }
  }

  async function captureWithDriver() {
    const sdk = await getSdk();
    if (!sdk) throw new Error('Cua Driver SDK is unavailable.');
    const activeDriver = await getDriver();
    if (!activeDriver) throw new Error('Cua Driver SDK is unavailable.');
    const result = await timeoutAfter(activeDriver.getDesktopState(
      sdk.GetDesktopStateInput.new({}),
    ), CAPTURE_TIMEOUT_MS);
    if (result.isError) throw new Error(result.text || 'Cua Driver could not capture the desktop.');
    const image = result.images?.[0];
    if (!image?.dataBase64) throw new Error('Cua Driver returned no screenshot image.');

    const display = screen.getPrimaryDisplay();
    const logicalWidth = display.size.width;
    const logicalHeight = display.size.height;
    const scaleFactor = display.scaleFactor || 1;
    const dimensions = imageDimensions(image.dataBase64, image.mimeType || 'image/png');
    if (!dimensions) throw new Error('Cua Driver screenshot did not include readable image dimensions.');
    return {
      data: image.dataBase64,
      mimeType: image.mimeType || 'image/png',
      width: dimensions.width,
      height: dimensions.height,
      logicalWidth,
      logicalHeight,
      scaleFactor,
      capturedAt: new Date().toISOString(),
      context: `${boundedContext(result)}\n${LOOP_UI_CONTEXT_NOTE}`,
      backend: 'cua-driver',
    };
  }

  async function capturePrimaryDisplay() {
    if (!driverUnavailable && !driverDisabled) {
      try {
        const frame = await captureWithDriver();
        driverFailure = null;
        return frame;
      }
      catch (error) {
        driverDisabled = true;
        driverFailure = error?.message || String(error);
        if (!desktopCapturer) throw error;
      }
    }
    if (electronDisabled) throw new Error(electronFailure || 'Electron screen capture is disabled until permissions are retried.');
    if (!desktopCapturer) throw new Error(driverFailure || 'No screen capture backend is available.');
    try {
      return await captureWithElectron();
    } catch (error) {
      electronDisabled = true;
      electronFailure = error?.message || String(error);
      throw error;
    }
  }

  async function captureWithElectron() {
    const display = screen.getPrimaryDisplay();
    const logicalWidth = display.size.width;
    const logicalHeight = display.size.height;
    const scaleFactor = display.scaleFactor || 1;
    const sources = await timeoutAfter(desktopCapturer.getSources({
      types: ['screen'],
      thumbnailSize: {
        width: Math.round(logicalWidth * scaleFactor),
        height: Math.round(logicalHeight * scaleFactor),
      },
      fetchWindowIcons: false,
    }), CAPTURE_TIMEOUT_MS);
    const source = sources.find((candidate) => candidate.display_id === String(display.id)) || sources[0];
    if (!source || !source.thumbnail || source.thumbnail.isEmpty()) {
      throw new Error('No primary display screenshot was returned. Check Screen Recording permission.');
    }
    const png = source.thumbnail.toPNG();
    const imageSize = nativeImage?.createFromBuffer
      ? nativeImage.createFromBuffer(png).getSize()
      : imageDimensions(png.toString('base64'), 'image/png');
    if (!imageSize) throw new Error('Electron screenshot did not include readable image dimensions.');
    return {
      data: png.toString('base64'), mimeType: 'image/png',
      width: imageSize.width, height: imageSize.height, logicalWidth, logicalHeight, scaleFactor,
      capturedAt: new Date().toISOString(),
      context: `${driverFailure ? `Cua Driver failed; using Electron fallback: ${driverFailure}. ` : ''}${LOOP_UI_CONTEXT_NOTE}`,
      backend: 'electron-desktopCapturer',
    };
  }

  function refresh() {
    if (stopping) return Promise.reject(new Error('Screen capture has stopped.'));
    if (refreshInFlight) return refreshInFlight;
    const operation = (async () => {
      if (stopping) throw new Error('Screen capture has stopped.');
      latestFrame = null;
      status('capturing', 'Capturing the primary display.', driverDisabled || driverUnavailable ? 'electron-desktopCapturer' : 'cua-driver');
      try {
        const frame = await capturePrimaryDisplay();
        if (stopping) throw new Error('Screen capture has stopped.');
        latestFrame = frame;
        const readyMessage = frame.backend === 'electron-desktopCapturer' && driverFailure
          ? `Screen context ready via Electron; Cua Driver failed: ${driverFailure.slice(0, 180)}`
          : `Screen context ready via ${frame.backend}.`;
        status('ready', readyMessage, frame.backend);
        return frame;
      } catch (error) {
        latestFrame = null;
        status('error', error?.message || String(error), driverFailure ? 'electron-desktopCapturer' : 'cua-driver');
        throw error;
      }
    })();
    refreshInFlight = operation.finally(() => { refreshInFlight = null; });
    return refreshInFlight;
  }

  function start() {
    if (timer || stopping) return;
    status('permission', 'Screen capture may require Screen Recording permission.');
    void refresh().catch(() => undefined);
    timer = setInterval(() => {
      if (!refreshInFlight) void refresh().catch(() => undefined);
    }, CAPTURE_INTERVAL_MS);
    timer.unref?.();
  }

  function stop() {
    stopping = true;
    if (timer) clearInterval(timer);
    timer = null;
    latestFrame = null;
    // The connected daemon is shared with the installed Driver app. Leave its lifecycle alone.
  }

  async function setupPermissions() {
    if (platform !== 'darwin') {
      status('permission', 'Grant screen capture permission in the operating system settings.');
      return false;
    }

    status('permission', 'Requesting macOS screen capture permission.');
    let grantFailure = null;
    try {
      await permissionGrant();
    } catch (error) {
      grantFailure = [error?.message, error?.stderr].filter(Boolean).join('\n') || String(error);
    }
    if (refreshInFlight) await refreshInFlight.catch(() => undefined);
    if (driver) {
      retiredDrivers.push(driver);
      driver = null;
    }
    driverDisabled = false;
    driverUnavailable = false;
    driverFailure = null;
    electronDisabled = false;
    electronFailure = null;
    latestFrame = null;
    try {
      await refresh();
      return true;
    } catch (error) {
      const captureFailure = error?.message || String(error);
      const detail = [grantFailure && `Driver setup: ${grantFailure}`, `Capture: ${captureFailure}`]
        .filter(Boolean).join(' | ').slice(0, 400);
      status('permission', detail || 'Screen capture is unavailable; check Screen Recording permission.');
      return false;
    }
  }

  return {
    start,
    stop,
    refresh,
    setupPermissions,
    getLatestFrame: () => latestFrame,
  };
}

module.exports = { createCapture, CAPTURE_INTERVAL_MS };
