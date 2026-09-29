/**
 * 生成最终图标 + 小尺寸清晰度验证。
 *
 * 只渲染 1024 是不够的：图标真正被看到的地方是任务栏（16-24px）与
 * 开始菜单（32-48px）。因此这里把候选渲成这些尺寸并排放在一起看。
 */

import { Canvas, sdPolyline, coverage, hex, mix, encodePng } from './renderer.mjs';
import { writeFileSync } from 'node:fs';

const BLUE = hex('#0A84FF');
const DEEP = hex('#0A6BE0');
const WHITE = hex('#FFFFFF');

function sdSquircle(px, py, cx, cy, r, n = 5.0) {
  const ax = Math.abs(px - cx) / r;
  const ay = Math.abs(py - cy) / r;
  const k = Math.pow(Math.pow(ax, n) + Math.pow(ay, n), 1 / n);
  return (k - 1) * r;
}

function strokePolyline(c, S, pts, width, color) {
  const px = 1 / S;
  const hw = width / 2;
  const ends = [pts[0], pts[pts.length - 1]];
  c.fill((x, y) => {
    const nx = (x + 0.5) * px, ny = (y + 0.5) * px;
    let d = sdPolyline(nx, ny, pts) - hw;
    for (const e of ends) d = Math.min(d, Math.hypot(nx - e[0], ny - e[1]) - hw);
    const a = coverage(d, px);
    return a <= 0 ? null : [color[0], color[1], color[2], color[3] * a];
  });
}

/**
 * 渲染图标。
 *
 * @param {number} S 尺寸
 * @param {object} o {
 *   width,        // 笔画粗细（归一化）
 *   inset,        // 对勾整体内缩，越大留白越多
 *   radius,       // 底板圆角半径（归一化，0.5 = 撑满）
 *   monochrome,   // true = 透明底 + 单色标记（Windows 单色模式用）
 * }
 */
function renderIcon(S, o) {
  const c = new Canvas(S);
  const px = 1 / S;
  const { width = 0.126, radius = 0.436, monochrome = false } = o;
  // inset 通过缩放对勾的控制点实现
  const k = o.inset ?? 1;

  // 对勾三点（R4 的夹角，再做 inset 缩放）
  const cx = 0.5, cy = 0.505;
  const raw = [
    [0.268, 0.512],
    [0.424, 0.672],
    [0.742, 0.336],
  ];
  const pts = raw.map(([x, y]) => [cx + (x - cx) * k + 0.012, cy + (y - cy) * k - 0.012]);

  if (!monochrome) {
    c.fill((x, y) => {
      const nx = (x + 0.5) * px, ny = (y + 0.5) * px;
      const d = sdSquircle(nx, ny, 0.5, 0.5, radius);
      const a = coverage(d, px);
      if (a <= 0) return null;
      const col = mix(BLUE, DEEP, nx * 0.35 + ny * 0.65);
      return [col[0], col[1], col[2], 255 * a];
    });
  }

  strokePolyline(c, S, pts, width, monochrome ? BLUE : WHITE);
  return c;
}

// ---------------------------------------------------------------------------
// 小尺寸清晰度对比：三档笔画粗细 × 四种实际尺寸
// ---------------------------------------------------------------------------
const configs = [
  { name: 'W1-thin', width: 0.112 },
  { name: 'W2-mid', width: 0.126 },
  { name: 'W3-bold', width: 0.142 },
];
const sizes = [16, 20, 24, 32, 48, 64];

const cellW = 96, cellH = 96;
const gridW = cellW * sizes.length;
const gridH = cellH * configs.length;
const grid = new Uint8ClampedArray(gridW * gridH * 4);

configs.forEach((cfg, row) => {
  sizes.forEach((S, col) => {
    const c = renderIcon(S, cfg);
    const offX = col * cellW + Math.floor((cellW - S) / 2);
    const offY = row * cellH + Math.floor((cellH - S) / 2);
    for (let y = 0; y < S; y++) {
      for (let x = 0; x < S; x++) {
        const si = (y * S + x) * 4;
        const sa = c.data[si + 3] / 255;
        if (sa === 0) continue;
        const di = ((y + offY) * gridW + (x + offX)) * 4;
        const da = grid[di + 3] / 255;
        const oa = sa + da * (1 - sa);
        grid[di] = (c.data[si] * sa + grid[di] * da * (1 - sa)) / oa;
        grid[di + 1] = (c.data[si + 1] * sa + grid[di + 1] * da * (1 - sa)) / oa;
        grid[di + 2] = (c.data[si + 2] * sa + grid[di + 2] * da * (1 - sa)) / oa;
        grid[di + 3] = oa * 255;
      }
    }
  });
});

writeFileSync('out/SMALL-SIZES.png', encodePng(grid, gridW, gridH));
console.log(`小尺寸对比图: out/SMALL-SIZES.png`);
console.log(`  行 = 笔画粗细 ${configs.map((c) => c.width).join(' / ')}`);
console.log(`  列 = ${sizes.join(' / ')} px`);

// ---------------------------------------------------------------------------
// 单色版（Windows 在某些位置会以单色渲染图标）
// ---------------------------------------------------------------------------
const mono = renderIcon(256, { width: 0.13, monochrome: true });
mono.save('out/MONO-256.png');
console.log('单色版: out/MONO-256.png');
