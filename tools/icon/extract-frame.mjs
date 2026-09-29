// 直接从 ICO 里解出指定尺寸的帧，放大后另存 ——
// 绕开 .NET Icon 类（它对 PNG 压缩的 ICO 帧处理有 bug，会返回彩色噪声）。

import { readFileSync, writeFileSync } from 'node:fs';
import { inflateSync } from 'node:zlib';
import { encodePng } from './renderer.mjs';

const buf = readFileSync(process.argv[2]);
const wantSize = Number(process.argv[3] || 16);
const zoom = Number(process.argv[4] || 8);
const outPath = process.argv[5] || 'out/frame.png';

const count = buf.readUInt16LE(4);
let chosen = null;
for (let i = 0; i < count; i++) {
  const off = 6 + i * 16;
  let w = buf[off] || 256;
  const size = buf.readUInt32LE(off + 8);
  const dataOff = buf.readUInt32LE(off + 12);
  if (w === wantSize) {
    chosen = { w, size, dataOff };
    break;
  }
}
if (!chosen) throw new Error(`找不到 ${wantSize}px 的帧`);

// 帧数据是完整 PNG 文件
const png = buf.subarray(chosen.dataOff, chosen.dataOff + chosen.size);

// 解析这个内嵌 PNG
let pos = 8, w = 0, h = 0;
const idat = [];
while (pos < png.length) {
  const len = png.readUInt32BE(pos);
  const type = png.toString('ascii', pos + 4, pos + 8);
  const data = png.subarray(pos + 8, pos + 8 + len);
  if (type === 'IHDR') { w = data.readUInt32BE(0); h = data.readUInt32BE(4); }
  if (type === 'IDAT') idat.push(data);
  pos += 12 + len;
}
const raw = inflateSync(Buffer.concat(idat));
const stride = w * 4;
const px = Buffer.alloc(w * h * 4);

// PNG 逐行滤波。必须实现全部 5 种 —— Tauri 生成的 PNG 会用到 2（Up）
// 与 4（Paeth）。只支持 0（None）在读取自己的输出时能过，
// 读别人的文件就会失败。
const paeth = (a, b, c) => {
  const p = a + b - c;
  const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
  return pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
};

let prevRow = Buffer.alloc(stride); // 上一行（已反滤波）
for (let y = 0; y < h; y++) {
  const base = y * (stride + 1);
  const filter = raw[base];
  const row = Buffer.from(raw.subarray(base + 1, base + 1 + stride));

  for (let i = 0; i < stride; i++) {
    const a = i >= 4 ? row[i - 4] : 0; // 左
    const b = prevRow[i]; // 上
    const c = i >= 4 ? prevRow[i - 4] : 0; // 左上
    let v = row[i];
    switch (filter) {
      case 0: break;
      case 1: v = (v + a) & 0xff; break;
      case 2: v = (v + b) & 0xff; break;
      case 3: v = (v + ((a + b) >> 1)) & 0xff; break;
      case 4: v = (v + paeth(a, b, c)) & 0xff; break;
      default: throw new Error(`未知滤波类型 ${filter}`);
    }
    row[i] = v;
  }

  row.copy(px, y * stride);
  prevRow = row;
}
console.log(`取出 ${w}x${h} 帧，滤波类型 ${new Set([...Array(h)].map((_, y) => raw[y * (stride + 1)])).size} 种，${idat.length} 个 IDAT 块`);

// 放大（最近邻），放在棋盘格背景上以看清透明区域
const Z = zoom;
const W = w * Z, H = h * Z;
const out = new Uint8ClampedArray(W * H * 4);
for (let y = 0; y < H; y++) {
  for (let x = 0; x < W; x++) {
    const sx = Math.floor(x / Z), sy = Math.floor(y / Z);
    const si = (sy * w + sx) * 4;
    const a = px[si + 3] / 255;
    // 棋盘格：浅灰/白交替，用来暴露透明像素
    const checker = (Math.floor(x / 8) + Math.floor(y / 8)) % 2 === 0 ? 200 : 235;
    const di = (y * W + x) * 4;
    out[di] = px[si] * a + checker * (1 - a);
    out[di + 1] = px[si + 1] * a + checker * (1 - a);
    out[di + 2] = px[si + 2] * a + checker * (1 - a);
    out[di + 3] = 255;
  }
}
writeFileSync(outPath, encodePng(out, W, H));
console.log(`已放大 ${Z} 倍并保存: ${outPath}  (${W}x${H}，棋盘格显示透明区)`);

// 顺便统计不透明像素占比，确认图标没被裁掉
let opaque = 0, total = 0;
for (let i = 3; i < px.length; i += 4) { total++; if (px[i] > 128) opaque++; }
console.log(`  不透明像素占比: ${((opaque / total) * 100).toFixed(1)}%`);
