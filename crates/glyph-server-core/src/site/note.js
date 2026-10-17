// The Glyph note in ASCII, as in the app's splash (src/ui/splash.rs in
// glyphmusic/desktop): characters stream in from the edges in curved
// currents and assemble the note; then a few characters keep flickering.
const NOTE_ART = [
  "                         ..+.. ",
  "                       ----#+ .",
  "                  ...-++-++++. ",
  "              ..-----+####+0.  ",
  "           .--.-++++##++++++.  ",
  "         +++++++++++---.. ++.  ",
  "        .+++++++.+....   .#+.  ",
  "        +0...+.++.        ++.  ",
  "        +++...            .+.  ",
  "        .+.               ++.  ",
  "        .+.               .0.  ",
  "        .+.               .+.  ",
  "        .+.               .+.  ",
  "        .+.               .+.  ",
  "        .+.               .+.  ",
  "        .+.               ++.  ",
  "        .+.           ... .+.  ",
  "        .+.         ..++++.+.  ",
  "        .+.        .+.++++0..  ",
  "        .+.       ...-+#+++0+  ",
  "  -++++ .+.       +.+####++++  ",
  " .+.+++#.+.       ..++.0++++.  ",
  ".+++###+0+.        +..+.+++.   ",
  ".+.+###++0+         ....+..    ",
  "..+###+++++           ...      ",
  "..+..0++++.                    ",
  " ....++++.                     ",
  "  ....+..                      ",
  "    ...                        ",
];
// p = pink, w = white.
const NOTE_TINT = [
  "                         wwwww ",
  "                       wwwwpp w",
  "                  wwwwwwwpwwww ",
  "              wwwwwwwpppppwww  ",
  "           wwwwwwwpppppwwwwww  ",
  "         wwwpwppppppwwwww www  ",
  "        wwwpppwwwwwwww   wwww  ",
  "        wwwwwwwwww        wpw  ",
  "        wwwwww            www  ",
  "        www               www  ",
  "        www               www  ",
  "        www               www  ",
  "        wpw               wpw  ",
  "        www               www  ",
  "        www               www  ",
  "        wpw               wpw  ",
  "        www           www www  ",
  "        www         wwwwwwwww  ",
  "        wpw        wwwpppwwww  ",
  "        wpw       wwwwpppwwww  ",
  "  wwwww www       wwwwpppppww  ",
  " wwwwppwwww       wwwwwwwppww  ",
  "wwwwpppwwww        wwwwwwpww   ",
  "wwwppppwwww         wwwwwww    ",
  "wwwwpppppww           www      ",
  "wwwwwwppwww                    ",
  " wwwwwwwww                     ",
  "  wwwwwww                      ",
  "    www                        ",
];

// canvas: fills its CSS box; the note is drawn centred, rowH px per row.
function glyphNote(canvas, rowH) {
  const CELL_ASPECT = 25.2 / 19.85, STREAMS = 6;
  const rnd = (i, salt) => {
    let h = (Math.imul(i, 0x9e3779b9) ^ Math.imul(salt, 0x85ebca6b)) >>> 0;
    h = (h ^ (h >>> 15)) >>> 0;
    h = Math.imul(h, 0x2c1b3c6d) >>> 0;
    h = (h ^ (h >>> 12)) >>> 0;
    return (h & 0xffff) / 65535;
  };
  const easeInOut = t => t < 0.5 ? 4 * t * t * t : 1 - Math.pow(-2 * t + 2, 3) / 2;
  const ps = [];
  let i = 0;
  NOTE_ART.forEach((line, row) => [...line].forEach((ch, col) => {
    if (ch === " ") return;
    const stream = Math.floor(rnd(i, 1) * STREAMS) % STREAMS;
    const base = stream / STREAMS * Math.PI * 2 + 0.4;
    ps.push({
      ch, pink: NOTE_TINT[row][col] === "p", col, row,
      angle: base + (rnd(i, 2) - 0.5) * 0.35,
      lane: rnd(i, 3) - 0.5,
      swirl: (stream % 2 === 0 ? 0.9 : -0.9) * (0.75 + 0.5 * rnd(stream, 7)),
      delay: stream * 0.07 + rnd(i, 4) * 0.6,
      dur: 0.85 + rnd(i, 5) * 0.35,
      glitch: 0, alt: ch,
    });
    i++;
  }));
  const settled = Math.max(...ps.map(p => p.delay + p.dur));
  const still = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const ctx = canvas.getContext("2d");
  const start = performance.now();
  const FLICKER = ".-+#0";

  function frame(now) {
    const dpr = window.devicePixelRatio || 1;
    const w = canvas.clientWidth, h = canvas.clientHeight;
    if (canvas.width !== Math.round(w * dpr) || canvas.height !== Math.round(h * dpr)) {
      canvas.width = Math.round(w * dpr); canvas.height = Math.round(h * dpr);
    }
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, w, h);
    const t = still ? settled : (now - start) / 1000;
    const cellH = rowH, cellW = rowH / CELL_ASPECT;
    const cols = NOTE_ART[0].length, rows = NOTE_ART.length;
    const cx = w / 2, cy = h / 2;
    const left = cx - cols * cellW / 2, top = cy - rows * cellH / 2;
    const reach = Math.max(w, h) * 0.78;
    ctx.font = `700 ${rowH * 0.95}px "JetBrains Mono", Consolas, Menlo, monospace`;
    ctx.textBaseline = "top";
    const at = (p, e) => {
      const a = p.angle;
      const sx = cx + reach * Math.cos(a) + Math.cos(a + Math.PI / 2) * p.lane * 120;
      const sy = cy + reach * Math.sin(a) + Math.sin(a + Math.PI / 2) * p.lane * 120;
      const ca = a + p.swirl;
      const kx = cx + reach * 0.55 * Math.cos(ca), ky = cy + reach * 0.55 * Math.sin(ca);
      const tx = left + p.col * cellW, ty = top + p.row * cellH, u = 1 - e;
      return [u * u * sx + 2 * u * e * kx + e * e * tx, u * u * sy + 2 * u * e * ky + e * e * ty];
    };
    // After assembly a few characters flicker, like the app's decode reveal.
    if (!still && t > settled && Math.random() < 0.18) {
      const p = ps[Math.floor(Math.random() * ps.length)];
      p.alt = FLICKER[Math.floor(Math.random() * FLICKER.length)];
      p.glitch = t + 0.12 + Math.random() * 0.2;
    }
    for (const p of ps) {
      const raw = Math.min(1, Math.max(0, (t - p.delay) / p.dur));
      if (raw <= 0) continue;
      const e = easeInOut(raw);
      const trail = raw < 1 ? [[0.07, 0.18], [0.035, 0.4], [0, 1]] : [[0, 1]];
      const glitching = p.glitch > t;
      for (const [back, alpha] of trail) {
        const [x, y] = at(p, Math.max(0, e - back));
        ctx.globalAlpha = alpha;
        ctx.fillStyle = p.pink || glitching ? "#ff5fd2" : "#f5f5f5";
        ctx.fillText(glitching ? p.alt : p.ch, x, y);
      }
    }
    ctx.globalAlpha = 1;
    if (!still) requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
  if (still) addEventListener("resize", () => requestAnimationFrame(frame));
}
