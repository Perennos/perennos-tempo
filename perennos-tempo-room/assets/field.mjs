// A slow field of points behind the insignia on Room's join and empty screens (three.js, WebGL only).
// It is decoration: it mounts only when WebGL works and motion is welcome, pauses when the tab or the
// canvas is hidden, follows the theme's colours, and never takes a click. No inline styles are written
// (the page's CSP allows none): the canvas is sized by its sheet, the renderer is told not to touch it.
const COLS = 64, ROWS = 40;

const vert = `
uniform float uTime; uniform float uPx; uniform vec2 uLook;
attribute float aSeed;
varying float vRipple; varying float vFade;
void main(){
  vec3 p = position;
  float d = length(p.xz);
  float w = sin(p.x*.55 + uTime*.35) * .5 + sin(p.z*.7 - uTime*.28) * .5;
  p.y += w * .55 + sin(aSeed*6.283 + uTime*.4) * .08;
  float front = fract(uTime*.06);
  vRipple = smoothstep(.09, .0, abs(d/14. - front));
  vec4 mv = modelViewMatrix * vec4(p, 1.);
  mv.xy += uLook * mv.z * -.02;
  gl_Position = projectionMatrix * mv;
  gl_PointSize = (2.4 + vRipple * 3.2 + aSeed * 1.2) * uPx * (9. / -mv.z);
  vFade = smoothstep(16., 3., d) * .85 + .15;
}`;
const frag = `
uniform vec3 uInk; uniform vec3 uWarm; uniform float uAlpha;
varying float vRipple; varying float vFade;
void main(){
  float r = length(gl_PointCoord - .5);
  if (r > .5) discard;
  float soft = smoothstep(.5, .1, r);
  vec3 c = mix(uInk, uWarm, clamp(vRipple * 1.4, 0., 1.));
  gl_FragColor = vec4(c, soft * vFade * (uAlpha + vRipple * .5));
}`;

const hex = (s, d) => {
  const m = String(s).trim().match(/^#([0-9a-f]{6})$/i);
  if (!m) return d;
  const v = parseInt(m[1], 16);
  return [(v >> 16) / 255, ((v >> 8) & 255) / 255, (v & 255) / 255];
};

export async function mountField(canvas) {
  if (!canvas || matchMedia('(prefers-reduced-motion: reduce)').matches) return null;
  const probe = document.createElement('canvas');
  if (!(probe.getContext('webgl2') || probe.getContext('webgl'))) return null;
  const THREE = await import('/vendor/three.module.min.js');
  let renderer;
  try {
    renderer = new THREE.WebGLRenderer({ canvas, antialias: false, alpha: true, powerPreference: 'low-power' });
  } catch { return null; }
  renderer.setClearColor(0x000000, 0);
  const scene = new THREE.Scene();
  const camera = new THREE.PerspectiveCamera(48, 1, .1, 80);
  camera.position.set(0, 4.2, 12);
  camera.lookAt(0, -.6, 0);
  const n = COLS * ROWS, pos = new Float32Array(n * 3), seed = new Float32Array(n);
  for (let i = 0; i < ROWS; i++) for (let j = 0; j < COLS; j++) {
    const k = i * COLS + j;
    pos[k * 3] = (j / (COLS - 1) - .5) * 40;
    pos[k * 3 + 1] = 0;
    pos[k * 3 + 2] = (i / (ROWS - 1) - .5) * 24;
    seed[k] = Math.abs(Math.sin(k * 12.9898) * 43758.5453) % 1;
  }
  const geo = new THREE.BufferGeometry();
  geo.setAttribute('position', new THREE.BufferAttribute(pos, 3));
  geo.setAttribute('aSeed', new THREE.BufferAttribute(seed, 1));
  const mat = new THREE.ShaderMaterial({
    vertexShader: vert, fragmentShader: frag, transparent: true, depthWrite: false,
    uniforms: {
      uTime: { value: 0 }, uPx: { value: 1 }, uLook: { value: new THREE.Vector2() },
      uInk: { value: new THREE.Vector3(.9, .9, .9) }, uWarm: { value: new THREE.Vector3(1, .36, .12) }, uAlpha: { value: .5 },
    },
  });
  scene.add(new THREE.Points(geo, mat));

  const root = document.documentElement;
  const paint = () => {
    const cs = getComputedStyle(root);
    const light = cs.colorScheme.trim() === 'light';
    mat.uniforms.uInk.value.set(...(light ? [.18, .17, .17] : hex(cs.getPropertyValue('--silver'), [.66, .67, .68])));
    mat.uniforms.uWarm.value.set(...hex(cs.getPropertyValue('--warm'), [1, .36, .12]));
    mat.uniforms.uAlpha.value = light ? .75 : .7;
  };
  const size = () => {
    const w = canvas.clientWidth, h = canvas.clientHeight;
    if (!w || !h) return false;
    const dpr = Math.min(devicePixelRatio || 1, 2);
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
      renderer.setPixelRatio(dpr);
      renderer.setSize(w, h, false);
      camera.aspect = w / h;
      camera.updateProjectionMatrix();
      mat.uniforms.uPx.value = dpr;
    }
    return true;
  };
  const look = { x: 0, y: 0, tx: 0, ty: 0 };
  addEventListener('pointermove', (e) => { look.tx = e.clientX / innerWidth - .5; look.ty = e.clientY / innerHeight - .5; }, { passive: true });
  let raf = 0, last = 0, t = 0, frames = 0;
  const run = (now) => {
    raf = requestAnimationFrame(run);
    if (!size()) return;
    const dt = Math.min(.05, (now - last) / 1000 || 0); last = now;
    t += dt;
    look.x += (look.tx - look.x) * .04; look.y += (look.ty - look.y) * .04;
    mat.uniforms.uTime.value = t;
    mat.uniforms.uLook.value.set(look.x, look.y);
    camera.position.x = look.x * 1.6;
    camera.lookAt(0, -.6, 0);
    if (frames++ % 90 === 0) paint();
    renderer.render(scene, camera);
  };
  const start = () => { if (!raf && !document.hidden) { last = performance.now(); raf = requestAnimationFrame(run); } };
  const stop = () => { cancelAnimationFrame(raf); raf = 0; };
  document.addEventListener('visibilitychange', () => (document.hidden ? stop() : start()));
  if ('IntersectionObserver' in window) new IntersectionObserver((es) => (es[0].isIntersecting ? start() : stop())).observe(canvas);
  canvas.addEventListener('webglcontextlost', (e) => { e.preventDefault(); stop(); });
  paint();
  canvas.classList.add('on');
  start();
  return { stop, start };
}
