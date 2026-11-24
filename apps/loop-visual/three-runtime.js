import * as THREE from 'three';
import { OrbitControls } from 'three/addons/controls/OrbitControls.js';
window.THREE = THREE;
window.OrbitControls = OrbitControls;
window.createWorld = function (container, options = {}) {
  const scene = new THREE.Scene();
  scene.background = new THREE.Color(options.background || '#101c28');
  const camera = new THREE.PerspectiveCamera(45, 1, 0.1, 1000);
  camera.position.set(7, 5, 9);
  const renderer = new THREE.WebGLRenderer({ antialias: true });
  renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
  renderer.outputColorSpace = THREE.SRGBColorSpace;
  container.appendChild(renderer.domElement);
  const controls = new OrbitControls(camera, renderer.domElement);
  controls.enableDamping = true;
  scene.add(new THREE.HemisphereLight(0xdceeff, 0x28303b, 2));
  const light = new THREE.DirectionalLight(0xffffff, 3);
  light.position.set(5, 8, 6); scene.add(light);
  const resize = new ResizeObserver(() => {
    const w = Math.max(1, container.clientWidth), h = Math.max(1, container.clientHeight);
    renderer.setSize(w, h); camera.aspect = w / h; camera.updateProjectionMatrix();
  });
  resize.observe(container);
  let update = () => {};
  renderer.setAnimationLoop((time) => { update(time / 1000); controls.update(); renderer.render(scene, camera); });
  addEventListener('pagehide', () => {
    renderer.setAnimationLoop(null); resize.disconnect(); controls.dispose();
    scene.traverse(o => { o.geometry?.dispose(); const materials = Array.isArray(o.material) ? o.material : [o.material]; materials.forEach(m => m?.dispose()); });
    renderer.dispose();
  }, { once: true });
  return { scene, camera, renderer, controls, animate(fn) { update = fn; } };
};
