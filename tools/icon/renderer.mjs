/**
 * 极简 2D 渲染器 + PNG 编码器。
 *
 * 为什么手写而不是装 sharp / Pillow：
 *   图标只需生成一次，为它引入一个带原生扩展的图像库并不划算 ——
 *   那会同时增加安装体积、平台兼容风险与供应链面。
 *   PNG 的格式本身很简单（zlib 压缩的像素 + CRC 校验），
 *   Node 内置 zlib 就够了。
 *
 * 抗锯齿策略：**每个像素做 4×4 超采样**，而不是靠 SVG 光栅化。
 * 这样形状数学完全由我们自己控制，不依赖任何渲染后端的实现差异。
 */

import { deflateSync } from 'node:zlib';
import { writeFileSync } from 'node:fs';

// ---------------------------------------------------------------------------
// PNG 编码
// ---------------------------------------------------------------------------

/** CRC32 查表（PNG 每个 chunk 都要带）。 */
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

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const typeBuf = Buffer.from(type, 'ascii');
  const body = Buffer.concat([typeBuf, data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body), 0);
  return Buffer.concat([len, body, crc]);
}

/**
 * 把 RGBA 像素缓冲编码成 PNG。
 *
 * @param {Uint8ClampedArray} rgba 长度 = w*h*4
 */
export function encodePng(rgba, w, h) {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(w, 0);
  ihdr.writeUInt32BE(h, 4);
  ihdr[8] = 8; // 位深 8
  ihdr[9] = 6; // 颜色类型 6 = RGBA
  ihdr[10] = 0; // 压缩方法
  ihdr[11] = 0; // 滤波方法
  ihdr[12] = 0; // 非隔行

  // 每行前面加一个滤波类型字节。用 0（None）：
  // 我们的图像是大面积纯色 + 少量边缘，用 Paeth 之类的滤波收益很小，
  // 而类型 0 让编码逻辑保持简单可读。
  const stride = w * 4;
  const raw = Buffer.alloc((stride + 1) * h);
  for (let y = 0; y < h; y++) {
    raw[y * (stride + 1)] = 0;
    Buffer.from(rgba.buffer, rgba.byteOffset + y * stride, stride).copy(
      raw,
      y * (stride + 1) + 1
    );
  }

  return Buffer.concat([
    sig,
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
}

// ---------------------------------------------------------------------------
// 颜色
// ---------------------------------------------------------------------------

/** '#RRGGBB' 或 '#RRGGBBAA' → [r,g,b,a]（0-255） */
export function hex(s) {
  const t = s.replace('#', '');
  const r = parseInt(t.slice(0, 2), 16);
  const g = parseInt(t.slice(2, 4), 16);
  const b = parseInt(t.slice(4, 6), 16);
  const a = t.length >= 8 ? parseInt(t.slice(6, 8), 16) : 255;
  return [r, g, b, a];
}

/** 线性混合两个颜色，t=0 取 a，t=1 取 b。 */
export function mix(a, b, t) {
  return [
    Math.round(a[0] + (b[0] - a[0]) * t),
    Math.round(a[1] + (b[1] - a[1]) * t),
    Math.round(a[2] + (b[2] - a[2]) * t),
    Math.round(a[3] + (b[3] - a[3]) * t),
  ];
}

// ---------------------------------------------------------------------------
// 画布
// ---------------------------------------------------------------------------

export class Canvas {
  constructor(size) {
    this.size = size;
    this.data = new Uint8ClampedArray(size * size * 4); // 全透明
  }

  /**
   * 逐像素填充：`fn(x, y)` 返回 [r,g,b,a] 或 null（表示不画）。
   *
   * 用逐像素而不是"画形状"，是因为形状都由有符号距离函数表达，
   * 统一在像素循环里求值才能拿到一致的抗锯齿结果。
   */
  fill(fn) {
    const { size, data } = this;
    for (let y = 0; y < size; y++) {
      for (let x = 0; x < size; x++) {
        const c = fn(x, y);
        if (!c || c[3] === 0) continue;
        const i = (y * size + x) * 4;
        const srcA = c[3] / 255;
        const dstA = data[i + 3] / 255;
        // 标准 source-over 合成
        const outA = srcA + dstA * (1 - srcA);
        if (outA === 0) continue;
        data[i] = (c[0] * srcA + data[i] * dstA * (1 - srcA)) / outA;
        data[i + 1] = (c[1] * srcA + data[i + 1] * dstA * (1 - srcA)) / outA;
        data[i + 2] = (c[2] * srcA + data[i + 2] * dstA * (1 - srcA)) / outA;
        data[i + 3] = outA * 255;
      }
    }
  }

  save(path) {
    writeFileSync(path, encodePng(this.data, this.size, this.size));
  }
}

// ---------------------------------------------------------------------------
// 有符号距离函数（SDF）
//
// 全部以**归一化坐标**（0..1）表达，与实际像素尺寸无关，
// 这样同一份形状定义可以直接渲成任意大小而不需要重算系数。
// ---------------------------------------------------------------------------

/** 圆角矩形。x,y 为归一化中心；返回带符号距离（负=内部，以归一化单位计）。 */
export function sdRoundRect(px, py, cx, cy, hw, hh, r) {
  const qx = Math.abs(px - cx) - (hw - r);
  const qy = Math.abs(py - cy) - (hh - r);
  const ax = Math.max(qx, 0);
  const ay = Math.max(qy, 0);
  return Math.hypot(ax, ay) + Math.min(Math.max(qx, qy), 0) - r;
}

/** 点到线段的距离。 */
function sdSegment(px, py, ax, ay, bx, by) {
  const vx = bx - ax;
  const vy = by - ay;
  const wx = px - ax;
  const wy = py - ay;
  const len2 = vx * vx + vy * vy;
  const t = len2 === 0 ? 0 : Math.max(0, Math.min(1, (wx * vx + wy * vy) / len2));
  return Math.hypot(wx - vx * t, wy - vy * t);
}

/** 折线（对每段取最小距离）。 */
export function sdPolyline(px, py, pts) {
  let d = Infinity;
  for (let i = 0; i < pts.length - 1; i++) {
    d = Math.min(d, sdSegment(px, py, pts[i][0], pts[i][1], pts[i + 1][0], pts[i + 1][1]));
  }
  return d;
}

/**
 * 极坐标「环扇形」：距圆心 ringR 半径 tol 内、且角度落在 [a0,a1] 的带。
 * 用于画时钟刻度、圆形进度、秒针弧线。
 */
export function sdArc(px, py, cx, cy, ringR, tol, a0, a1) {
  const dx = px - cx;
  const dy = py - cy;
  const r = Math.hypot(dx, dy);
  const dr = Math.abs(r - ringR) - tol;
  if (dr > 0) return dr; // 半径就不在环上，快速排除

  // 角度归一化到 [0, 2π)
  const TAU = Math.PI * 2;
  let ang = Math.atan2(dy, dx);
  if (ang < 0) ang += TAU;
  let s = a0 % TAU;
  let e = a1 % TAU;
  if (s < 0) s += TAU;
  if (e < 0) e += TAU;

  const inRange = s <= e ? ang >= s && ang <= e : ang >= s || ang <= e;
  if (inRange) return dr;
  // 不在角度范围内：返回环边缘到角度边界的最近距离（近似，够用于抗锯齿）
  return Math.abs(dr);
}

/** 圆。 */
export function sdCircle(px, py, cx, cy, r) {
  return Math.hypot(px - cx, py - cy) - r;
}

/**
 * 把 SDF 距离转成覆盖率（0..1）。
 *
 * `px` 是"一个像素在归一化坐标下的长度"，用它做 1 像素左右的过渡带。
 * 这就是抗锯齿的本质：边缘处按距离线性过渡，而不是硬切。
 */
export function coverage(d, px) {
  return Math.max(0, Math.min(1, 0.5 - d / px));
}
