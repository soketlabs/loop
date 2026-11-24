'use strict';
const { contextBridge, ipcRenderer } = require('electron');
if (process.isMainFrame) {
  contextBridge.exposeInMainWorld('loopVisual', {
    prompt: (text) => ipcRenderer.invoke('visual:prompt', text),
    cancel: () => ipcRenderer.invoke('visual:cancel'),
    closeVisual: () => ipcRenderer.invoke('visual:close'),
    setupPermissions: () => ipcRenderer.invoke('visual:permissions'),
    onEvent: (fn) => {
      const listener = (_, value) => fn(value);
      ipcRenderer.on('visual:event', listener);
      ipcRenderer.send('visual:listen');
      return () => ipcRenderer.removeListener('visual:event', listener);
    },
  });
}
