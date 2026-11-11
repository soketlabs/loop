'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const { createCapture, CAPTURE_INTERVAL_MS } = require('./capture.cjs');

function fakeImage() {
  const bytes = Buffer.alloc(24);
  Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]).copy(bytes);
  bytes.writeUInt32BE(2880, 16);
  bytes.writeUInt32BE(1800, 20);
  return {
    isEmpty: () => false,
    toPNG: () => bytes,
    getSize: () => ({ width: 2880, height: 1800 }),
  };
}

test('refresh returns retina metadata without changing window state', async () => {
  let windowReads = 0;
  let windowChanges = 0;
  const statuses = [];
  const window = {
    isDestroyed: () => { windowReads += 1; return false; },
    isVisible: () => { windowReads += 1; return true; },
    isFocused: () => { windowReads += 1; return true; },
    getOpacity: () => { windowReads += 1; return 0.85; },
    hide: () => { windowChanges += 1; },
    showInactive: () => { windowChanges += 1; },
    setOpacity: () => { windowChanges += 1; },
  };
  const capture = createCapture({
    driverSDK: false,
    desktopCapturer: { getSources: async () => [{ display_id: '1', thumbnail: fakeImage() }] },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 1440, height: 900 }, scaleFactor: 2 }) },
    windows: () => { windowReads += 1; return [window]; },
    onStatus: (state) => statuses.push(state),
  });

  const frame = await capture.refresh();
  assert.equal(frame.width, 2880);
  assert.equal(frame.height, 1800);
  assert.equal(frame.logicalWidth, 1440);
  assert.equal(frame.scaleFactor, 2);
  assert.equal(frame.mimeType, 'image/png');
  assert.equal(frame.backend, 'electron-desktopCapturer');
  assert.equal(Buffer.from(frame.data, 'base64').readUInt32BE(16), 2880);
  assert.equal(windowReads, 0);
  assert.equal(windowChanges, 0);
  assert.match(frame.context, /Loop Visual panels may occlude desktop content/);
  assert.equal(statuses.at(-1).state, 'ready');
  assert.equal(CAPTURE_INTERVAL_MS, 3000);
  capture.stop();
});

test('capture failure leaves window visibility and opacity untouched', async () => {
  let windowReads = 0;
  let windowChanges = 0;
  const capture = createCapture({
    driverSDK: false,
    desktopCapturer: { getSources: async () => [] },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 100, height: 80 }, scaleFactor: 1 }) },
    windows: () => { windowReads += 1; return [{
      isDestroyed: () => { windowReads += 1; return false; },
      isVisible: () => { windowReads += 1; return true; },
      getOpacity: () => { windowReads += 1; return 1; },
      hide: () => { windowChanges += 1; },
      showInactive: () => { windowChanges += 1; },
      setOpacity: () => { windowChanges += 1; },
    }]; },
  });

  await assert.rejects(capture.refresh(), /No primary display screenshot/);
  assert.equal(windowReads, 0);
  assert.equal(windowChanges, 0);
  capture.stop();
});

test('focused window opacity and visibility stay stable during capture', async () => {
  const opacity = 0.85;
  const focused = true;
  let reads = 0;
  let changes = 0;
  const window = {
    isDestroyed: () => { reads += 1; return false; },
    isVisible: () => { reads += 1; return true; },
    isFocused: () => { reads += 1; return focused; },
    getOpacity: () => { reads += 1; return opacity; },
    setOpacity: () => { changes += 1; },
    hide: () => { changes += 1; },
    showInactive: () => { changes += 1; },
  };
  const capture = createCapture({
    driverSDK: false,
    desktopCapturer: { getSources: async () => {
      assert.equal(opacity, 0.85);
      assert.equal(focused, true);
      return [{ display_id: '1', thumbnail: fakeImage() }];
    } },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 100, height: 80 }, scaleFactor: 1 }) },
    windows: () => [window],
  });

  await capture.refresh();
  assert.equal(opacity, 0.85);
  assert.equal(focused, true);
  assert.equal(reads, 0);
  assert.equal(changes, 0);
  capture.stop();
});

test('stopping during capture leaves window state untouched and rejects late frame', async () => {
  let windowChanges = 0;
  let finishCapture;
  const capture = createCapture({
    driverSDK: false,
    desktopCapturer: { getSources: () => new Promise((resolve) => { finishCapture = resolve; }) },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 100, height: 80 }, scaleFactor: 1 }) },
    windows: () => [{
      isDestroyed: () => false,
      isVisible: () => true,
      getOpacity: () => 1,
      hide: () => { windowChanges += 1; },
      showInactive: () => { windowChanges += 1; },
      setOpacity: () => { windowChanges += 1; },
    }],
  });

  const pending = capture.refresh();
  await new Promise((resolve) => setImmediate(resolve));
  capture.stop();
  finishCapture([{ display_id: '1', thumbnail: fakeImage() }]);
  await assert.rejects(pending, /Screen capture has stopped/);
  assert.equal(windowChanges, 0);
});

test('Driver failure disables Driver retries and reports Electron fallback details', async () => {
  let driverCalls = 0;
  const statuses = [];
  const capture = createCapture({
    driverSDK: {
      CuaDriver: { connect: () => ({ getDesktopState: async () => { driverCalls += 1; throw new Error('Driver daemon unavailable'); } }) },
      GetDesktopStateInput: { new: (value) => value },
    },
    desktopCapturer: { getSources: async () => [{ display_id: '1', thumbnail: fakeImage() }] },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 1440, height: 900 }, scaleFactor: 2 }) },
    onStatus: (state) => statuses.push(state),
  });

  const first = await capture.refresh();
  const second = await capture.refresh();
  assert.equal(first.backend, 'electron-desktopCapturer');
  assert.match(first.context, /Driver daemon unavailable/);
  assert.equal(second.backend, 'electron-desktopCapturer');
  assert.equal(driverCalls, 1);
  assert.equal(statuses.at(-1).backend, 'electron-desktopCapturer');
  assert.match(statuses.at(-1).message, /Driver daemon unavailable/);
  capture.stop();
});

test('concurrent refresh requests coalesce instead of queuing captures', async () => {
  let calls = 0;
  let resolveSources;
  const capture = createCapture({
    driverSDK: false,
    desktopCapturer: { getSources: () => { calls += 1; return new Promise((resolve) => { resolveSources = resolve; }); } },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 1440, height: 900 }, scaleFactor: 2 }) },
  });
  const first = capture.refresh();
  await new Promise((resolve) => setImmediate(resolve));
  const second = capture.refresh();
  assert.equal(first, second);
  resolveSources([{ display_id: '1', thumbnail: fakeImage() }]);
  await first;
  assert.equal(calls, 1);
  capture.stop();
});

test('permission setup retries Electron capture when Driver grant command fails', async () => {
  let grantCalls = 0;
  const capture = createCapture({
    platform: 'darwin',
    permissionGrant: async () => { grantCalls += 1; throw new Error('Driver permission flow unavailable'); },
    driverSDK: false,
    desktopCapturer: { getSources: async () => [{ display_id: '1', thumbnail: fakeImage() }] },
    screen: { getPrimaryDisplay: () => ({ id: 1, size: { width: 1440, height: 900 }, scaleFactor: 2 }) },
  });

  assert.equal(await capture.setupPermissions(), true);
  assert.equal(grantCalls, 1);
  assert.equal(capture.getLatestFrame().backend, 'electron-desktopCapturer');
  capture.stop();
});
