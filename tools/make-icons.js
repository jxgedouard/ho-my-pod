// Generates icons/icon.png and icons/icon.ico (a HomePod seen from above,
// with the colourful Siri glow). No dependencies: rasterises with 4x4
// supersampling and writes PNG with node's zlib.
// Usage: node tools/make-icons.js

const fs = require("fs");
const path = require("path");
const zlib = require("zlib");

function crc32(buf) {
  let c, crc = 0xffffffff;
  for (let n = 0; n < buf.length; n++) {
    c = (crc ^ buf[n]) & 0xff;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    crc = (crc >>> 8) ^ c;
  }
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

function png(size, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const raw = Buffer.alloc((size * 4 + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0;
    rgba.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", zlib.deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

const mix = (a, b, t) => a + (b - a) * t;
const clamp = (x) => Math.max(0, Math.min(1, x));

// Colour (premultiplied-free RGBA in 0..1) of the icon at (u, v) in [-1, 1]
function shade(u, v) {
  const r = Math.hypot(u, v);
  const R = 0.92;
  if (r > R) return [0, 0, 0, 0];
  // Fabric body: soft light-grey dome with top-left highlight
  const light = clamp(0.62 + 0.38 * (1 - Math.hypot(u + 0.35, v + 0.4) / 1.6));
  let col = [mix(0.70, 0.98, light), mix(0.70, 0.98, light), mix(0.73, 0.99, light)];
  // Subtle mesh texture
  const mesh = 0.025 * Math.sin((u + v) * 60) * Math.sin((u - v) * 60);
  col = col.map((c) => c + mesh);
  // Rim shadow
  const rim = clamp((r - 0.80) / (R - 0.80));
  col = col.map((c) => c * (1 - 0.35 * rim * rim));
  // Top glass disc
  const disc = 0.58;
  if (r < disc) {
    const t = r / disc;
    col = [mix(0.08, 0.12, t), mix(0.08, 0.12, t), mix(0.10, 0.14, t)];
    // Siri glow: blend of pink / violet / blue around the centre
    const ang = Math.atan2(v, u);
    const glow = Math.exp(-Math.pow(r / 0.36, 2) * 1.6);
    const c1 = [1.0, 0.27, 0.55]; // pink
    const c2 = [0.55, 0.36, 1.0]; // violet
    const c3 = [0.2, 0.68, 1.0]; // blue
    const w1 = 0.5 + 0.5 * Math.cos(ang - 0.6);
    const w2 = 0.5 + 0.5 * Math.cos(ang - 2.7);
    const w3 = 0.5 + 0.5 * Math.cos(ang + 1.6);
    const sum = w1 + w2 + w3;
    const g = [0, 1, 2].map((i) => (c1[i] * w1 + c2[i] * w2 + c3[i] * w3) / sum);
    col = col.map((c, i) => mix(c, g[i], clamp(glow * 1.15)));
    // Glass edge highlight
    const edge = clamp(1 - Math.abs(r - disc + 0.012) / 0.012);
    col = col.map((c) => c + 0.25 * edge * clamp(-v + 0.2));
  }
  return [clamp(col[0]), clamp(col[1]), clamp(col[2]), 1];
}

function render(size) {
  const ss = 4;
  const buf = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let acc = [0, 0, 0, 0];
      for (let sy = 0; sy < ss; sy++) {
        for (let sx = 0; sx < ss; sx++) {
          const u = ((x + (sx + 0.5) / ss) / size) * 2 - 1;
          const v = ((y + (sy + 0.5) / ss) / size) * 2 - 1;
          const c = shade(u, v);
          acc[0] += c[0] * c[3];
          acc[1] += c[1] * c[3];
          acc[2] += c[2] * c[3];
          acc[3] += c[3];
        }
      }
      const n = ss * ss;
      const a = acc[3] / n;
      const i = (y * size + x) * 4;
      buf[i] = a > 0 ? Math.round((acc[0] / acc[3]) * 255) : 0;
      buf[i + 1] = a > 0 ? Math.round((acc[1] / acc[3]) * 255) : 0;
      buf[i + 2] = a > 0 ? Math.round((acc[2] / acc[3]) * 255) : 0;
      buf[i + 3] = Math.round(a * 255);
    }
  }
  return png(size, buf);
}

function ico(images) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(images.length, 4);
  const entries = [];
  let offset = 6 + 16 * images.length;
  for (const { size, data } of images) {
    const e = Buffer.alloc(16);
    e[0] = size >= 256 ? 0 : size;
    e[1] = size >= 256 ? 0 : size;
    e.writeUInt16LE(1, 4); // planes
    e.writeUInt16LE(32, 6); // bpp
    e.writeUInt32LE(data.length, 8);
    e.writeUInt32LE(offset, 12);
    offset += data.length;
    entries.push(e);
  }
  return Buffer.concat([header, ...entries, ...images.map((i) => i.data)]);
}

const outDir = path.join(__dirname, "..", "icons");
fs.mkdirSync(outDir, { recursive: true });
const sizes = [16, 24, 32, 48, 64, 128, 256];
const images = sizes.map((size) => ({ size, data: render(size) }));
fs.writeFileSync(path.join(outDir, "icon.png"), render(512));
fs.writeFileSync(path.join(outDir, "32x32.png"), images.find((i) => i.size === 32).data);
fs.writeFileSync(path.join(outDir, "128x128.png"), images.find((i) => i.size === 128).data);
fs.writeFileSync(path.join(outDir, "icon.ico"), ico(images));
console.log("icons written to", outDir);
