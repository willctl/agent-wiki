// Generates the tray icons from the SVG sources in assets/icon/:
//   dist/icons/agent-wiki-<state>.ico   16, 20, 24, 32, 48, 64, 256 px (BMP frames up to 64, PNG at 256)
//   dist/icons/agent-wiki-<state>.svg   the source with that state's colors applied
//   dist/icons/preview.png              every state and size on light and dark taskbars, plus 16 px at 8x
// States: healthy, degraded (amber badge), down (grey page, red badge).
//
// No dependencies: a small renderer for the SVG subset the sources use (rect with
// rx, circle, path with M/L/H/V/C/S/Q/Z, class and display attributes, and the
// knockout circle of a <defs> mask), scanline coverage anti-aliasing, and PNG/ICO
// encoders on node:zlib.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import zlib from 'node:zlib';

const root = fileURLToPath(new URL('..', import.meta.url));
const SRC = path.join(root, 'assets', 'icon');
const OUT = path.join(root, 'dist', 'icons');

/** tile: the page (class "tile"); accent: its folded corner (class "accent"); badge: the status dot. */
export const STATES = {
  healthy: { tile: '#4F46E5', accent: '#A5B4FC', badge: null },
  degraded: { tile: '#4F46E5', accent: '#A5B4FC', badge: '#F59E0B' },
  down: { tile: '#6B7280', accent: '#D1D5DB', badge: '#DC2626' },
};
export const SIZES = [16, 20, 24, 32, 48, 64, 256];
/** Which source draws each size: the pixel-fitted design at 16 px; the full one above (class "detail" only from 32 px). */
const sourceFor = (size) => (size <= 16 ? { file: 'agent-wiki-16.svg', detail: true } : { file: 'agent-wiki.svg', detail: size >= 32 });

// ---------------------------------------------------------------- SVG subset

function attrs(s) {
  const a = {};
  for (const m of s.matchAll(/([\w:-]+)="([^"]*)"/g)) a[m[1]] = m[2];
  return a;
}

/** Parses the source into [{cls, fill, polys}] in paint order, applying a state's colors. */
export function parseSvg(text, state, { detail = true } = {}) {
  const vb = attrs(text.match(/<svg\b[^>]*>/)[0]).viewBox.split(/\s+/).map(Number);
  const shape = (tag, a, classes) => {
    let polys;
    if (tag === 'rect') polys = [roundRect(+a.x || 0, +a.y || 0, +a.width, +a.height, +(a.rx || 0))];
    else if (tag === 'circle') polys = [ellipse(+a.cx, +a.cy, +a.r)];
    else polys = pathPolys(a.d);
    return { classes, fill: a.fill || '#000000', polys };
  };
  const clean = text.replace(/<!--[\s\S]*?-->/g, '');
  // <defs> is not painted. Its mask's knockout circle (the ring cut around the badge) is erased just before the badge.
  const knockouts = [...(clean.match(/<defs>[\s\S]*?<\/defs>/)?.[0] ?? '').matchAll(/<circle\b([^>]*?)\/?>/g)]
    .map((m) => attrs(m[1]))
    .filter((a) => (a.class || '').split(/\s+/).includes('knockout'))
    .map((a) => shape('circle', a, ['badge', 'knockout']));
  const shapes = [];
  const stack = [];
  for (const m of clean.replace(/<defs>[\s\S]*?<\/defs>/, '').matchAll(/<(\/?)(g|rect|circle|path)\b([^>]*?)(\/?)>/g)) {
    const [, close, tag, rest, selfClose] = m;
    if (close) {
      stack.pop();
      continue;
    }
    const a = attrs(rest);
    const classes = [...stack.flatMap((g) => g.classes), ...(a.class || '').split(/\s+/).filter(Boolean)];
    const hiddenByDefault = stack.some((g) => g.display === 'none') || a.display === 'none';
    if (tag === 'g') {
      if (!selfClose) stack.push({ classes: (a.class || '').split(/\s+/).filter(Boolean), display: a.display });
      continue;
    }
    // State rules: the badge shows only when the state has one; the page, its fold and the dot take the state's colors.
    const isBadge = classes.includes('badge');
    if (isBadge ? !state.badge : hiddenByDefault) continue;
    if (!detail && classes.includes('detail')) continue;
    if (isBadge && knockouts.length) shapes.push(...knockouts.splice(0));
    const s = shape(tag, a, classes);
    if (classes.includes('tile') || classes.includes('lines')) s.fill = state.tile;
    if (classes.includes('accent') && state.accent) s.fill = state.accent;
    if (classes.includes('badge-dot')) s.fill = state.badge;
    shapes.push(s);
  }
  return { viewBox: vb, shapes };
}

/** The source as a standalone SVG in a state's colors; with a badge, the glyph takes the knockout mask. */
export function stateSvg(text, colors) {
  let s = text.replace(/#4F46E5/g, colors.tile).replace(/#A5B4FC/g, colors.accent);
  if (colors.badge) {
    s = s
      .replace('<g class="badge" display="none">', '<g class="badge">')
      .replace('fill="#F59E0B"', `fill="${colors.badge}"`)
      .replace('<g class="glyph">', '<g class="glyph" mask="url(#knockout)">');
  }
  return s;
}

function ellipse(cx, cy, r, n = 128) {
  return Array.from({ length: n }, (_, i) => [cx + r * Math.cos((2 * Math.PI * i) / n), cy + r * Math.sin((2 * Math.PI * i) / n)]);
}

function roundRect(x, y, w, h, r) {
  if (!r) return [[x, y], [x + w, y], [x + w, y + h], [x, y + h]];
  r = Math.min(r, w / 2, h / 2);
  const pts = [];
  const corner = (cx, cy, a0) => {
    for (let i = 0; i <= 16; i++) {
      const a = a0 + (Math.PI / 2) * (i / 16);
      pts.push([cx + r * Math.cos(a), cy + r * Math.sin(a)]);
    }
  };
  corner(x + w - r, y + r, -Math.PI / 2);
  corner(x + w - r, y + h - r, 0);
  corner(x + r, y + h - r, Math.PI / 2);
  corner(x + r, y + r, Math.PI);
  return pts;
}

function pathPolys(d) {
  const toks = d.match(/[a-zA-Z]|-?\d*\.?\d+(?:e[-+]?\d+)?/g);
  const polys = [];
  let cur = null;
  let x = 0;
  let y = 0;
  let sx = 0;
  let sy = 0;
  let lastCtrl = null;
  let i = 0;
  let cmd = '';
  const num = () => Number(toks[i++]);
  const cubic = (x1, y1, x2, y2, x3, y3) => {
    for (let k = 1; k <= 24; k++) {
      const t = k / 24;
      const u = 1 - t;
      cur.push([u * u * u * x + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3, u * u * u * y + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3]);
    }
    lastCtrl = [x2, y2];
    x = x3;
    y = y3;
  };
  while (i < toks.length) {
    if (/[a-zA-Z]/.test(toks[i])) cmd = toks[i++];
    const rel = cmd === cmd.toLowerCase();
    const ox = rel ? x : 0;
    const oy = rel ? y : 0;
    switch (cmd.toUpperCase()) {
      case 'M':
        x = ox + num();
        y = oy + num();
        cur = [[x, y]];
        polys.push(cur);
        sx = x;
        sy = y;
        cmd = rel ? 'l' : 'L';
        break;
      case 'L':
        x = ox + num();
        y = oy + num();
        cur.push([x, y]);
        break;
      case 'H':
        x = ox + num();
        cur.push([x, y]);
        break;
      case 'V':
        y = oy + num();
        cur.push([x, y]);
        break;
      case 'C':
        cubic(ox + num(), oy + num(), ox + num(), oy + num(), ox + num(), oy + num());
        continue;
      case 'S': {
        const [cx, cy] = lastCtrl ? [2 * x - lastCtrl[0], 2 * y - lastCtrl[1]] : [x, y];
        cubic(cx, cy, ox + num(), oy + num(), ox + num(), oy + num());
        continue;
      }
      case 'Q': {
        const qx = ox + num();
        const qy = oy + num();
        const ex = ox + num();
        const ey = oy + num();
        cubic(x + (2 / 3) * (qx - x), y + (2 / 3) * (qy - y), ex + (2 / 3) * (qx - ex), ey + (2 / 3) * (qy - ey), ex, ey);
        continue;
      }
      case 'Z':
        x = sx;
        y = sy;
        break;
      default:
        throw new Error(`unsupported path command ${cmd}`);
    }
    lastCtrl = null;
  }
  return polys;
}

// ---------------------------------------------------------------- raster

const hex = (c) => [1, 3, 5].map((i) => parseInt(c.slice(i, i + 2), 16) / 255);

/** Renders to straight (non-premultiplied) RGBA bytes. Nonzero fill, 16 subsamples per pixel row, exact horizontal coverage. */
export function render(svg, size) {
  const [vx, vy, vw, vh] = svg.viewBox;
  const sc = size / vw;
  const S = 16;
  const acc = new Float64Array(size * size * 4); // premultiplied
  for (const shape of svg.shapes) {
    const cov = new Float64Array(size * size);
    const edges = [];
    for (const p of shape.polys) {
      for (let k = 0; k < p.length; k++) {
        const a = p[k];
        const b = p[(k + 1) % p.length];
        if (a[1] !== b[1]) edges.push([(a[0] - vx) * sc, (a[1] - vy) * sc, (b[0] - vx) * sc, (b[1] - vy) * sc]);
      }
    }
    for (let py = 0; py < size; py++) {
      for (let s = 0; s < S; s++) {
        const yy = py + (s + 0.5) / S;
        const xs = [];
        for (const [x0, y0, x1, y1] of edges) {
          if ((yy >= y0 && yy < y1) || (yy >= y1 && yy < y0)) xs.push([x0 + ((yy - y0) / (y1 - y0)) * (x1 - x0), y1 > y0 ? 1 : -1]);
        }
        xs.sort((p, q) => p[0] - q[0]);
        let wind = 0;
        for (let k = 0; k < xs.length - 1; k++) {
          wind += xs[k][1];
          if (!wind) continue;
          const a = Math.max(0, xs[k][0]);
          const b = Math.min(size, xs[k + 1][0]);
          for (let px = Math.floor(a); px < Math.ceil(b); px++) cov[py * size + px] += (Math.min(b, px + 1) - Math.max(a, px)) / S;
        }
      }
    }
    const [r, g, b] = hex(shape.fill);
    const knockout = shape.classes.includes('knockout'); // erases what is below (destination-out)
    for (let k = 0; k < size * size; k++) {
      const al = Math.min(1, cov[k]);
      if (!al) continue;
      const o = k * 4;
      if (knockout) {
        for (let c = 0; c < 4; c++) acc[o + c] *= 1 - al;
        continue;
      }
      acc[o] = r * al + acc[o] * (1 - al);
      acc[o + 1] = g * al + acc[o + 1] * (1 - al);
      acc[o + 2] = b * al + acc[o + 2] * (1 - al);
      acc[o + 3] = al + acc[o + 3] * (1 - al);
    }
  }
  const out = Buffer.alloc(size * size * 4);
  for (let k = 0; k < size * size; k++) {
    const a = acc[k * 4 + 3];
    for (let c = 0; c < 3; c++) out[k * 4 + c] = a ? Math.round(Math.min(1, acc[k * 4 + c] / a) * 255) : 0;
    out[k * 4 + 3] = Math.round(a * 255);
  }
  return out;
}

// ---------------------------------------------------------------- PNG + ICO

const CRC = new Uint32Array(256).map((_, n) => {
  let c = n;
  for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
  return c >>> 0;
});
function crc32(buf) {
  let c = 0xffffffff;
  for (const b of buf) c = CRC[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

export function png(rgba, w, h = w) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8;
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc((w * 4 + 1) * h);
  for (let y = 0; y < h; y++) rgba.copy(raw, y * (w * 4 + 1) + 1, y * w * 4, (y + 1) * w * 4);
  return Buffer.concat([Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]), chunk('IHDR', ihdr), chunk('IDAT', zlib.deflateSync(raw, { level: 9 })), chunk('IEND', Buffer.alloc(0))]);
}

/** A 32-bit DIB icon frame: BITMAPINFOHEADER, BGRA bottom-up, and an all-zero AND mask (alpha does the work). */
function dib(rgba, size) {
  const head = Buffer.alloc(40);
  head.writeUInt32LE(40, 0);
  head.writeInt32LE(size, 4);
  head.writeInt32LE(size * 2, 8);
  head.writeUInt16LE(1, 12);
  head.writeUInt16LE(32, 14);
  head.writeUInt32LE(size * size * 4, 20);
  const px = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const s = (y * size + x) * 4;
      const d = ((size - 1 - y) * size + x) * 4;
      px[d] = rgba[s + 2];
      px[d + 1] = rgba[s + 1];
      px[d + 2] = rgba[s];
      px[d + 3] = rgba[s + 3];
    }
  }
  const maskRow = Math.ceil(size / 32) * 4;
  return Buffer.concat([head, px, Buffer.alloc(maskRow * size)]);
}

export function ico(frames) {
  const head = Buffer.alloc(6 + 16 * frames.length);
  head.writeUInt16LE(0, 0);
  head.writeUInt16LE(1, 2);
  head.writeUInt16LE(frames.length, 4);
  let offset = head.length;
  const blobs = frames.map(({ size, rgba }) => (size >= 256 ? png(rgba, size) : dib(rgba, size)));
  frames.forEach(({ size }, i) => {
    const e = 6 + i * 16;
    head[e] = size >= 256 ? 0 : size;
    head[e + 1] = size >= 256 ? 0 : size;
    head.writeUInt16LE(1, e + 4);
    head.writeUInt16LE(32, e + 6);
    head.writeUInt32LE(blobs[i].length, e + 8);
    head.writeUInt32LE(offset, e + 12);
    offset += blobs[i].length;
  });
  return Buffer.concat([head, ...blobs]);
}

// ---------------------------------------------------------------- preview sheet

function sheet(images) {
  // Rows: states. Columns: sizes on a light and a dark taskbar color, then 16 px at 8x.
  const pad = 12;
  const zoom = 8;
  const colW = SIZES.map((s) => s + pad);
  const half = colW.reduce((a, b) => a + b, pad);
  const W = half * 2 + 16 * zoom + pad * 2;
  const rowH = Math.max(256, 16 * zoom) + pad * 2;
  const H = rowH * Object.keys(STATES).length;
  const out = Buffer.alloc(W * H * 4);
  const fill = (x0, y0, w, h, rgb) => {
    for (let y = y0; y < y0 + h; y++) for (let x = x0; x < x0 + w; x++) out.set([...rgb, 255], (y * W + x) * 4);
  };
  const blit = (img, size, x0, y0, z = 1) => {
    for (let y = 0; y < size * z; y++) {
      for (let x = 0; x < size * z; x++) {
        const s = ((Math.floor(y / z) * size + Math.floor(x / z)) * 4);
        const d = ((y0 + y) * W + x0 + x) * 4;
        const a = img[s + 3] / 255;
        for (let c = 0; c < 3; c++) out[d + c] = Math.round(img[s + c] * a + out[d + c] * (1 - a));
      }
    }
  };
  fill(0, 0, half, H, [243, 243, 243]);
  fill(half, 0, half, H, [32, 32, 32]);
  fill(half * 2, 0, W - half * 2, H, [128, 128, 128]);
  Object.keys(STATES).forEach((state, r) => {
    const y = r * rowH + pad;
    for (const bg of [0, half]) {
      let x = bg + pad;
      for (const s of SIZES) {
        blit(images[state][s], s, x, y + (rowH - pad * 2 - s) / 2);
        x += s + pad;
      }
    }
    blit(images[state][16], 16, half * 2 + pad, y, zoom);
  });
  return png(out, W, H);
}

// ---------------------------------------------------------------- main

export function buildIcons({ outDir = OUT, preview = true } = {}) {
  fs.mkdirSync(outDir, { recursive: true });
  const images = {};
  for (const [state, colors] of Object.entries(STATES)) {
    images[state] = {};
    const frames = SIZES.map((size) => {
      const src = sourceFor(size);
      const svg = parseSvg(fs.readFileSync(path.join(SRC, src.file), 'utf8'), colors, src);
      const rgba = render(svg, size);
      images[state][size] = rgba;
      return { size, rgba };
    });
    fs.writeFileSync(path.join(outDir, `agent-wiki-${state}.ico`), ico(frames));
    fs.writeFileSync(path.join(outDir, `agent-wiki-${state}.svg`), stateSvg(fs.readFileSync(path.join(SRC, 'agent-wiki.svg'), 'utf8'), colors));
    fs.writeFileSync(path.join(outDir, `agent-wiki-${state}-256.png`), png(images[state][256], 256));
  }
  if (preview) fs.writeFileSync(path.join(outDir, 'preview.png'), sheet(images));
  return Object.keys(STATES).map((s) => path.join(outDir, `agent-wiki-${s}.ico`));
}

if (process.argv[1] && fs.realpathSync(path.resolve(process.argv[1])) === fs.realpathSync(fileURLToPath(import.meta.url))) {
  for (const f of buildIcons()) console.log(`built ${path.relative(root, f).replace(/\\/g, '/')} (${fs.statSync(f).size} bytes)`);
}
