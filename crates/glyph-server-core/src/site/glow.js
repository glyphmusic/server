// The app's background, ported from glyph/src/visuals: the "Strahl" glow
// field (field.rs) on a low-res canvas the browser stretches into soft light,
// and the ASCII layer plus titlebar dot grid (glow.rs) on a full-res canvas.
// Same ramp, cell size, 5x7 font, thresholds and alphas as the app.
// Returns `{ palette(dark, mid, bright) }` to tint the field with other
// colours (a theme plugin's glow, `0xrrggbb` each).
function glyphGlow(glowEl, asciiEl, titlebarH) {
  const RAMP_LEN = 12; // " .-=+xX0S8#@"
  const FONT = [
    [0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 4, 0],
    [0, 0, 0, 14, 0, 0, 0],
    [0, 0, 31, 0, 31, 0, 0],
    [0, 4, 4, 31, 4, 4, 0],
    [0, 0, 17, 10, 4, 10, 17],
    [17, 17, 10, 4, 10, 17, 17],
    [14, 17, 19, 21, 25, 17, 14],
    [15, 16, 16, 14, 1, 1, 30],
    [14, 17, 17, 14, 17, 17, 14],
    [10, 10, 31, 10, 31, 10, 10],
    [14, 17, 23, 21, 23, 16, 15],
  ];
  const PITCH = 8, CW = 10, CH = 16;
  const hex = (h) => [(h >> 16 & 255) / 255, (h >> 8 & 255) / 255, (h & 255) / 255];
  const lerp = (a, b, t) => [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
  const scale = (a, k) => [a[0] * k, a[1] * k, a[2] * k];
  const lighten = (a, k) => [a[0] + (1 - a[0]) * k, a[1] + (1 - a[1]) * k, a[2] + (1 - a[2]) * k];
  // A pink palette in the app's palette ramp shape (field.rs `ramps`).
  let cool, warm;
  const ramps = (pa, pb, pd) => {
    cool = [scale(pa, 0.35), pa, pb, lighten(pb, 0.85)];
    warm = [scale(pa, 0.35), pb, pd, lighten(pd, 0.85)];
  };
  ramps(hex(0x6a2a5e), hex(0xd291c4), hex(0xf3d6ec));
  const at = (r, v) => { v = Math.min(1, Math.max(0, v)) * 3; const i = Math.min(2, v | 0); return lerp(r[i], r[i + 1], v - i); };
  const gauss = (x) => Math.exp(-x * x);
  const smooth = (a, b, x) => { const t = Math.min(1, Math.max(0, (x - a) / (b - a))); return t * t * (3 - 2 * t); };
  // Smooth 3D value noise in -1..1 (stands in for OpenSimplex).
  const hash = (x, y, z) => { let h = (x * 374761393 + y * 668265263 + z * 1274126177) | 0; h = (h ^ (h >>> 13)) * 1274126177 | 0; return ((h ^ (h >>> 16)) >>> 0) / 4294967295 * 2 - 1; };
  const fade = (t) => t * t * (3 - 2 * t);
  function noise(x, y, z) {
    const xi = Math.floor(x), yi = Math.floor(y), zi = Math.floor(z);
    const xf = fade(x - xi), yf = fade(y - yi), zf = fade(z - zi);
    const l = (a, b, t) => a + (b - a) * t;
    const c = (dx, dy, dz) => hash(xi + dx, yi + dy, zi + dz);
    return l(l(l(c(0, 0, 0), c(1, 0, 0), xf), l(c(0, 1, 0), c(1, 1, 0), xf), yf), l(l(c(0, 0, 1), c(1, 0, 1), xf), l(c(0, 1, 1), c(1, 1, 1), xf), yf), zf);
  }
  function strahl(x, y, aspect, m) {
    x -= m.dx; y -= m.dy;
    const ax = -0.05 * aspect, ay = 1.08, bx = aspect * 1.02, by = -0.08;
    const dx = bx - ax, dy = by - ay, len = Math.hypot(dx, dy), ux = dx / len, uy = dy / len;
    const px = x - ax, py = y - ay, s = (px * ux + py * uy) / len, d = Math.abs(px * uy - py * ux);
    const sc = Math.min(1, Math.max(0, s));
    const w = (0.02 + 0.17 * Math.pow(sc, 1.15)) * m.size;
    const along = smooth(-0.05, 0.25, s) * (0.55 + 0.45 * sc);
    return [(gauss(d / w) + gauss(d / (w * 3)) * 0.26) * along, smooth(0.35, 1, s)];
  }

  // Values of a playing app at default intensity (glow.rs `Job`): the page
  // is about listening, so the field looks like it does while music plays.
  const level = 1, density = 0.83, threshold = 0.42, dotLevel = 0.32;
  const gctx = glowEl.getContext("2d"), actx = asciiEl.getContext("2d");
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  let W = 0, H = 0, dpr = 1, iw = 0, ih = 0, samples = [], img = null, t0 = performance.now();

  function resize() {
    W = innerWidth; H = innerHeight; dpr = Math.max(1, Math.round(devicePixelRatio || 1));
    iw = Math.max(2, Math.ceil(W / PITCH)); ih = Math.max(2, Math.ceil(H / PITCH));
    glowEl.width = iw; glowEl.height = ih;
    asciiEl.width = W * dpr; asciiEl.height = H * dpr;
    img = gctx.createImageData(iw, ih);
    samples = new Array(iw * ih);
  }
  function draw() {
    const t = (performance.now() - t0) / 1000;
    const breath = Math.sin(2 * Math.PI * t / 28), drift = t / 34;
    // field.rs Motion::new with playing = 1 and soft average bands.
    const m = { dx: noise(drift, 3.7, 0) * 0.14, dy: noise(11.1, drift, 0) * 0.07, size: 1 + breath * 0.09 + 0.3 * 0.32, gain: 0.94 + 0.3 * 0.38 };
    const aspect = W / H, sh = 0.10 + 0.08 * 1;
    const data = img.data;
    for (let py = 0; py < ih; py++) {
      const y = (py + 0.5) / ih;
      for (let px = 0; px < iw; px++) {
        const x = (px + 0.5) / iw * aspect;
        let [b, s] = strahl(x, y, aspect, m);
        const n = noise(x * 1.7, y * 1.7, t * 0.045) * sh;
        b = Math.max(0, (b + n * Math.min(b, 1) * 1.4) * m.gain);
        const c = lerp(at(cool, b), at(warm, b), s);
        const k = smooth(0.02, 1.1, b) * level * 0.5, o = (py * iw + px) * 4;
        data[o] = c[0] * k * 255; data[o + 1] = c[1] * k * 255; data[o + 2] = c[2] * k * 255; data[o + 3] = 255;
        samples[py * iw + px] = [b, c];
      }
    }
    gctx.putImageData(img, 0, 0);

    actx.clearRect(0, 0, asciiEl.width, asciiEl.height);
    const p = dpr, sample = (lx, ly) => samples[Math.min(ih - 1, ly / PITCH | 0) * iw + Math.min(iw - 1, lx / PITCH | 0)];
    const cols = W / CW | 0, rows = (H / CH | 0) + 1;
    for (let row = 0; row < rows; row++) {
      const ly = row * CH;
      if (ly < titlebarH || ly + CH > H) continue;
      for (let col = 0; col < cols; col++) {
        const lx = col * CW;
        const [b, c] = sample(lx + CW / 2, ly + CH / 2);
        const dv = b * density;
        if (dv < threshold) continue;
        const idx = Math.min(RAMP_LEN - 1, Math.max(1, Math.round(Math.min(dv, 1) * (RAMP_LEN - 1))));
        const cc = lighten(c, 0.3);
        actx.fillStyle = `rgba(${cc[0] * 255 | 0},${cc[1] * 255 | 0},${cc[2] * 255 | 0},${(0.16 + 0.16 * Math.min(dv, 1)) * level})`;
        const ox = Math.floor((lx + (CW - 5) / 2) * dpr), oy = Math.floor((ly + (CH - 7) / 2) * dpr);
        const g = FONT[idx];
        for (let ry = 0; ry < 7; ry++) {
          const bits = g[ry];
          if (!bits) continue;
          for (let rx = 0; rx < 5; rx++) if (bits & (16 >> rx)) actx.fillRect(ox + rx * p, oy + ry * p, p, p);
        }
      }
    }
    // Titlebar dot grid: 2 px dots, 4 px pitch, glow colour along the top.
    const floor = hex(0x0c1a6b), dotsY = titlebarH / 4 | 0, d = Math.max(1, Math.round(2 * dpr));
    for (let i = 0; i < W / 4; i++) {
      const lx = i * 4 + 1, [b, c] = sample(lx, 6), tint = lerp(floor, c, smooth(0, 0.6, b));
      for (let k = 0; k < dotsY; k++) {
        actx.fillStyle = `rgba(${tint[0] * 255 | 0},${tint[1] * 255 | 0},${tint[2] * 255 | 0},${dotLevel * (1 - k / dotsY * 0.35)})`;
        actx.fillRect(lx * dpr | 0, (k * 4 + 1) * dpr | 0, d, d);
      }
    }
  }
  resize();
  draw();
  addEventListener("resize", () => { resize(); draw(); });
  const api = { palette(a, b, d) { ramps(hex(a), hex(b), hex(d)); draw(); } };
  if (reduce) return api;
  // The field runs at 15 fps here (the app: 30, ASCII 15); paused while hidden.
  setInterval(() => { if (!document.hidden) draw(); }, 1000 / 15);
  return api;
}
