// Minimal, dependency-free PNG read/resize/write for the MSIX logo assets.
//
// Scope is deliberately small: 8-bit, non-interlaced PNGs (greyscale, RGB,
// greyscale+alpha, RGBA — the icon sources in assets/ are 1024x1024 RGBA).
// Anything else fails loudly rather than producing a wrong image.

import { deflateSync, inflateSync } from "node:zlib";

const SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();

function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

/** Width/height from the IHDR chunk, without decoding pixels. */
export function readPngSize(buf) {
  if (!buf.subarray(0, 8).equals(SIGNATURE)) throw new Error("not a PNG file");
  if (buf.toString("latin1", 12, 16) !== "IHDR") throw new Error("PNG has no IHDR");
  return { width: buf.readUInt32BE(16), height: buf.readUInt32BE(20) };
}

/** Decode to { width, height, data: Uint8Array RGBA (straight alpha) }. */
export function decodePng(buf) {
  if (!buf.subarray(0, 8).equals(SIGNATURE)) throw new Error("not a PNG file");
  let pos = 8;
  let ihdr = null;
  const idat = [];
  while (pos < buf.length) {
    const len = buf.readUInt32BE(pos);
    const type = buf.toString("latin1", pos + 4, pos + 8);
    const data = buf.subarray(pos + 8, pos + 8 + len);
    pos += 12 + len;
    if (type === "IHDR") {
      ihdr = {
        width: data.readUInt32BE(0),
        height: data.readUInt32BE(4),
        depth: data[8],
        colorType: data[9],
        interlace: data[12],
      };
    } else if (type === "IDAT") {
      idat.push(data);
    } else if (type === "IEND") {
      break;
    }
  }
  if (!ihdr) throw new Error("PNG has no IHDR");
  const { width, height, depth, colorType, interlace } = ihdr;
  const channels = { 0: 1, 2: 3, 4: 2, 6: 4 }[colorType];
  if (depth !== 8 || !channels || interlace !== 0) {
    throw new Error(
      `unsupported PNG (bit depth ${depth}, colour type ${colorType}, interlace ${interlace}); ` +
        "export the icon source as an 8-bit non-interlaced RGBA PNG",
    );
  }
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const px = new Uint8Array(stride * height);
  let prev = new Uint8Array(stride);
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const out = px.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x++) {
      const a = x >= channels ? out[x - channels] : 0;
      const b = prev[x];
      const c = x >= channels ? prev[x - channels] : 0;
      let v = line[x];
      switch (filter) {
        case 0: break;
        case 1: v += a; break;
        case 2: v += b; break;
        case 3: v += (a + b) >> 1; break;
        case 4: {
          const p = a + b - c;
          const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
          v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
          break;
        }
        default: throw new Error(`bad PNG filter type ${filter}`);
      }
      out[x] = v & 0xff;
    }
    prev = out;
  }
  const rgba = new Uint8Array(width * height * 4);
  for (let i = 0; i < width * height; i++) {
    const s = i * channels;
    const d = i * 4;
    if (channels === 1 || channels === 2) {
      rgba[d] = rgba[d + 1] = rgba[d + 2] = px[s];
      rgba[d + 3] = channels === 2 ? px[s + 1] : 255;
    } else {
      rgba[d] = px[s]; rgba[d + 1] = px[s + 1]; rgba[d + 2] = px[s + 2];
      rgba[d + 3] = channels === 4 ? px[s + 3] : 255;
    }
  }
  return { width, height, data: rgba };
}

function chunk(type, data) {
  const head = Buffer.alloc(8);
  head.writeUInt32BE(data.length, 0);
  head.write(type, 4, "latin1");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([head.subarray(4), data])), 0);
  return Buffer.concat([head, data, crc]);
}

/** Encode RGBA { width, height, data } as an 8-bit RGBA PNG. */
export function encodePng({ width, height, data }) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = 0; ihdr[11] = 0; ihdr[12] = 0;
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (stride + 1)] = 0;
    Buffer.from(data.buffer, data.byteOffset + y * stride, stride).copy(raw, y * (stride + 1) + 1);
  }
  return Buffer.concat([
    SIGNATURE,
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

/**
 * Resize with an area-average (box) filter over premultiplied alpha, which is
 * what you want for shrinking a 1024px icon to 44px without dark fringes.
 */
export function resize(img, outW, outH) {
  const { width: inW, height: inH, data } = img;
  const out = new Uint8Array(outW * outH * 4);
  const sx = inW / outW;
  const sy = inH / outH;
  for (let oy = 0; oy < outH; oy++) {
    const y0 = oy * sy, y1 = y0 + sy;
    for (let ox = 0; ox < outW; ox++) {
      const x0 = ox * sx, x1 = x0 + sx;
      let r = 0, g = 0, b = 0, a = 0, wsum = 0;
      for (let iy = Math.floor(y0); iy < Math.min(Math.ceil(y1), inH); iy++) {
        const wy = Math.min(iy + 1, y1) - Math.max(iy, y0);
        for (let ix = Math.floor(x0); ix < Math.min(Math.ceil(x1), inW); ix++) {
          const w = (Math.min(ix + 1, x1) - Math.max(ix, x0)) * wy;
          const i = (iy * inW + ix) * 4;
          const al = data[i + 3] / 255;
          r += data[i] * al * w; g += data[i + 1] * al * w; b += data[i + 2] * al * w;
          a += al * w; wsum += w;
        }
      }
      const o = (oy * outW + ox) * 4;
      if (a > 0) {
        out[o] = Math.round(r / a); out[o + 1] = Math.round(g / a); out[o + 2] = Math.round(b / a);
      }
      out[o + 3] = Math.round((a / wsum) * 255);
    }
  }
  return { width: outW, height: outH, data: out };
}

/** Transparent canvas of the given size with `img` scaled to fit `scale` of the
 * shorter side and centred. */
export function fitCentered(img, canvasW, canvasH, scale) {
  const side = Math.max(1, Math.round(Math.min(canvasW, canvasH) * scale));
  const ratio = img.width / img.height;
  const w = ratio >= 1 ? side : Math.max(1, Math.round(side * ratio));
  const h = ratio >= 1 ? Math.max(1, Math.round(side / ratio)) : side;
  const small = resize(img, w, h);
  const out = new Uint8Array(canvasW * canvasH * 4);
  const left = Math.floor((canvasW - w) / 2);
  const top = Math.floor((canvasH - h) / 2);
  for (let y = 0; y < h; y++) {
    out.set(small.data.subarray(y * w * 4, (y + 1) * w * 4), ((top + y) * canvasW + left) * 4);
  }
  return { width: canvasW, height: canvasH, data: out };
}
