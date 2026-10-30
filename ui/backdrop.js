// Edge tracers for the hexagon field: comets that wander along the comb's edges, leaving a fading, glowing trail.
// Drawn on a canvas laid over the comb so the trail can fade continuously, which stacked SVG dashes cannot do.
(function () {
  'use strict';
  if (!window.matchMedia || matchMedia('(prefers-reduced-motion: reduce)').matches) return;
  const svg = document.querySelector('svg.backdrop');
  if (!svg) return;
  const canvas = document.createElement('canvas');
  canvas.className = 'backdrop';
  canvas.setAttribute('aria-hidden', 'true');
  svg.after(canvas);
  const ctx = canvas.getContext('2d');
  if (!ctx) return;

  // Comb geometry from #hx-comb: pointy-top cells of radius 28, rows 42 apart, odd rows shifted half a cell.
  const COL = 48.5, ROW = 42, HALF = 24.25, EDGE = 28;
  // Each vertex meets three edges; "up" vertices (a cell's top) branch up/down-left/down-right, "down" vertices the mirror.
  const UP = [[0, -EDGE], [-HALF, 14], [HALF, 14]];
  const DOWN = [[0, EDGE], [-HALF, -14], [HALF, -14]];
  const VIEW_W = 1440, VIEW_H = 900; // the backdrop's viewBox, sliced to the window from the top right
  const TRAIL = 150; // trail length, viewBox units
  const STEP = 3; // trail is drawn as segments this long, each a little fainter than the one ahead
  // Strokes drawn under each segment, widest first: [width in viewBox units, alpha].
  const PASSES = [[9, 0.02], [4.5, 0.045], [1.6, 0.45]];
  const SPEED = 240; // viewBox units per second
  const MAX = 6;

  const rand = (a, b) => a + Math.random() * (b - a);
  const key = (x, y) => Math.round(x * 4) + ',' + Math.round(y * 4);
  const color = getComputedStyle(document.documentElement).getPropertyValue('--color-accent').trim() || '#3fe0b5';

  let scale = 1, offsetX = 0, dpr = 1;
  function resize() {
    dpr = window.devicePixelRatio || 1;
    const w = innerWidth, h = innerHeight;
    canvas.width = Math.round(w * dpr);
    canvas.height = Math.round(h * dpr);
    // Match preserveAspectRatio="xMaxYMin slice" on the SVG.
    scale = Math.max(w / VIEW_W, h / VIEW_H);
    offsetX = w - VIEW_W * scale;
  }
  resize();
  addEventListener('resize', resize);

  // Same falloff as #hx-fade/#hx-mask: full at the top right, gone toward the bottom left.
  function fade(x, y) {
    const u = x / VIEW_W - 1, v = y / VIEW_H;
    const t = (u * -0.65 + v * 0.9) / (0.65 * 0.65 + 0.9 * 0.9);
    return 1 - Math.min(1, Math.max(0, t));
  }

  function walk() {
    // Start at a cell top in the visible (top-right) part of the field.
    const r = Math.floor(rand(0, 18));
    const c = Math.floor(rand(12, 30));
    let x = c * COL + (r & 1 ? HALF : 0), y = r * ROW - EDGE, up = true;
    const pts = [[x, y]], seen = new Set([key(x, y)]);
    const steps = Math.floor(rand(10, 26));
    for (let i = 0; i < steps; i++) {
      const opts = (up ? UP : DOWN)
        .map(([dx, dy]) => [x + dx, y + dy])
        .filter(([nx, ny]) => !seen.has(key(nx, ny)));
      if (!opts.length) break;
      [x, y] = opts[Math.floor(Math.random() * opts.length)];
      seen.add(key(x, y));
      pts.push([x, y]);
      up = !up;
    }
    return pts;
  }

  // Point at distance s along a walk; every edge is EDGE long.
  function at(pts, s) {
    const i = Math.min(pts.length - 2, Math.floor(s / EDGE));
    const f = s / EDGE - i;
    const a = pts[i], b = pts[i + 1];
    return [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f];
  }

  const comets = [];
  function spawn() {
    const pts = walk();
    comets.push({ pts, len: (pts.length - 1) * EDGE, head: 0 });
  }

  // One trail segment from distance s0 to s1, bending through any corner in between.
  function segment(c, s0, s1) {
    const [x0, y0] = at(c.pts, s0);
    ctx.moveTo(offsetX + x0 * scale, y0 * scale);
    for (let k = Math.floor(s0 / EDGE) + 1; k * EDGE < s1; k++) {
      ctx.lineTo(offsetX + c.pts[k][0] * scale, c.pts[k][1] * scale);
    }
    const [x1, y1] = at(c.pts, s1);
    ctx.lineTo(offsetX + x1 * scale, y1 * scale);
  }

  function draw(c) {
    const end = Math.min(c.head, c.len);
    const start = Math.max(0, c.head - TRAIL);
    for (const [width, alpha] of PASSES) {
      ctx.lineWidth = width * scale;
      for (let s = start; s < end; s += STEP) {
        const s1 = Math.min(end, s + STEP);
        const age = (c.head - s1) / TRAIL; // 0 at the head, 1 at the tail's end
        const [mx, my] = at(c.pts, s1);
        ctx.globalAlpha = alpha * Math.pow(1 - age, 2) * fade(mx, my);
        ctx.beginPath();
        segment(c, s, s1);
        ctx.stroke();
      }
    }
  }

  let last = 0;
  function frame(now) {
    // Cap the step so a backgrounded tab resumes where it left off instead of jumping.
    const dt = Math.min(0.05, (now - (last || now)) / 1000);
    last = now;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.clearRect(0, 0, canvas.width, canvas.height);
    ctx.strokeStyle = color;
    ctx.lineCap = 'butt';
    ctx.lineJoin = 'round';
    for (let i = comets.length - 1; i >= 0; i--) {
      const c = comets[i];
      c.head += SPEED * dt;
      if (c.head - TRAIL >= c.len) {
        comets.splice(i, 1);
        setTimeout(spawn, rand(400, 3200));
        continue;
      }
      draw(c);
    }
    requestAnimationFrame(frame);
  }

  for (let i = 0; i < MAX; i++) setTimeout(spawn, rand(0, 6000));
  requestAnimationFrame(frame);
})();
