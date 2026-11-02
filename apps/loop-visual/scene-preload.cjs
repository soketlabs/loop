'use strict';
const { contextBridge, ipcRenderer } = require('electron');
if (process.isMainFrame) {
  contextBridge.exposeInMainWorld('loopScene', {
    onRender: (fn) => {
      const listener = (_, value) => fn(value);
      ipcRenderer.on('scene:render', listener);
      return () => ipcRenderer.removeListener('scene:render', listener);
    },
    state: (sceneId, state) => ipcRenderer.send('scene:state', { sceneId, state }),
    ready: () => ipcRenderer.send('scene:ready'),
  });
}
