'use strict';
const frame = document.querySelector('#visual');
let sceneId = null;
window.addEventListener('message', (event) => {
  if (event.source !== frame.contentWindow || !event.data || event.data.kind !== 'loop-visual-state') return;
  if (event.data.sceneId !== sceneId) return;
  window.loopScene.state(sceneId, event.data.state);
});
window.loopScene.onRender((scene) => {
  sceneId = scene.sceneId;
  // srcdoc gets its parent's CSP and an opaque origin. The generated document never gets a preload.
  const bootstrap = `<script>window.visualState=function(state){parent.postMessage({kind:'loop-visual-state',sceneId:${JSON.stringify(sceneId)},state:state},'*')};<\/script>`;
  const runtime = scene.runtimeScript ? '<script>' + scene.runtimeScript.replace(/<\/script/gi, '<\\/script') + '<\/script>' : '';
  frame.srcdoc = bootstrap + runtime + scene.html;
});
window.loopScene.ready();
