/**
 * 生成 Todox 的最终图标源图（1024×1024）。
 *
 * 设计说明
 * --------
 * 构成：蓝色 squircle 底板 + 白色圆头对勾。
 *
 * 为什么是这个方案而不是更"有设计感"的方案（抽象标记、环形进度、
 * 打勾列表）：
 *
 *   1. **图标的第一职责是能被认出，不是表达个性。** 在任务栏 16px 下，
 *      任何超过两个视觉元素的构图都会糊成一团。实测过打勾列表（B）与
 *      环形进度（D），两者在 32px 以下都失去了可辨识度。
 *   2. **对勾是零学习成本的符号。** 用户不需要理解一个抽象标记，
 *      看到勾就知道这是"完成/待办"。
 *   3. **squircle 而不是圆角矩形**：超椭圆（n≈5）在视觉上更饱满、
 *      更接近系统组件的语言，同样的视觉重量下占地面积更大，
 *      因此小尺寸下更醒目。
 *   4. **笔画取中间档（0.126）**：实测 0.112 在 24px 以上偏细、
 *      0.142 在 16px 下糊成色块。0.126 是两端的平衡点。
 *   5. **渐变极克制**（#0A84FF → #0A6BE0，色相不变）：纯平色略显呆板，
 *      但强渐变会让小尺寸下的边缘发浑。只改明度、不改色相，
 *      既有体积感又不损失轮廓清晰度。
 *   6. **对勾做了光学居中**：机械居中时对勾会显得偏右下
 *      （因为视觉重量集中在左下的转折处），因此整体右移 0.012、
 *      上移 0.012 来补偿。
 *
 * 运行：node .icon-work/make-icon.mjs
 * 然后用 `pnpm tauri icon` 从 icon-source.png 生成全部规格。
 */

import { Canvas, sdPolyline, coverage, hex, mix } from './renderer.mjs';

const BLUE = hex('#0A84FF');
const DEEP = hex('#0A6BE0');
const WHITE = hex('#FFFFFF');

// 以下参数与 small-sizes.mjs 的验证保持一致
const STROKE = 0.126; // 对勾笔画粗细（归一化）
const RADIUS = 0.436; // 底板半径。0.5 会顶满画布没有留白
const OPTICAL_DX = 0.012; // 光学居中补偿
const OPTICAL_DY = -0.012;

/** 超椭圆的有符号距离近似。n 越大越接近圆角矩形，n=2 是正圆。 */
function sdSquircle(px, py, cx, cy, r, n = 5.0) {
  const ax = Math.abs(px - cx) / r;
  const ay = Math.abs(py - cy) / r;
  const k = Math.pow(Math.pow(ax, n) + Math.pow(ay, n), 1 / n);
  return (k - 1) * r;
}

/** 圆头折线。端点是整圆，转折处由距离场自然形成圆角。 */
function strokePolyline(c, S, pts, width, color) {
  const px = 1 / S;
  const hw = width / 2;
  const ends = [pts[0], pts[pts.length - 1]];
  c.fill((x, y) => {
    const nx = (x + 0.5) * px;
    const ny = (y + 0.5) * px;
    let d = sdPolyline(nx, ny, pts) - hw;
    for (const e of ends) d = Math.min(d, Math.hypot(nx - e[0], ny - e[1]) - hw);
    const a = coverage(d, px);
    return a <= 0 ? null : [color[0], color[1], color[2], color[3] * a];
  });
}

const S = 1024;
const c = new Canvas(S);
const px = 1 / S;

// ---- 1. 底板 ----
c.fill((x, y) => {
  const nx = (x + 0.5) * px;
  const ny = (y + 0.5) * px;
  const d = sdSquircle(nx, ny, 0.5, 0.5, RADIUS);
  const a = coverage(d, px);
  if (a <= 0) return null;
  // 只改明度的斜向渐变：左上略亮 → 右下略深
  const col = mix(BLUE, DEEP, nx * 0.35 + ny * 0.65);
  return [col[0], col[1], col[2], 255 * a];
});

// ---- 2. 对勾 ----
// 起笔短、收笔长，夹角约 52°。这是手写对勾的直觉比例：
// 两笔等长会显得机械，收笔过长会显得轻浮。
const pts = [
  [0.268 + OPTICAL_DX, 0.512 + OPTICAL_DY],
  [0.424 + OPTICAL_DX, 0.672 + OPTICAL_DY],
  [0.742 + OPTICAL_DX, 0.336 + OPTICAL_DY],
];
strokePolyline(c, S, pts, STROKE, WHITE);

c.save('icon-source.png');

// 校验：确认图形居中、背景透明
const at = (x, y) => {
  const i = (y * S + x) * 4;
  return [c.data[i], c.data[i + 1], c.data[i + 2], c.data[i + 3]];
};
let left = -1, right = -1, top = -1, bot = -1;
for (let x = 0; x < S; x++) if (at(x, S / 2)[3] > 128) { left = x; break; }
for (let x = S - 1; x >= 0; x--) if (at(x, S / 2)[3] > 128) { right = x; break; }
for (let y = 0; y < S; y++) if (at(S / 2, y)[3] > 128) { top = y; break; }
for (let y = S - 1; y >= 0; y--) if (at(S / 2, y)[3] > 128) { bot = y; break; }

console.log('icon-source.png 已生成 (1024x1024)');
console.log(`  水平: ${left}..${right}  宽 ${right - left + 1}  中心 ${(left + right) / 2}`);
console.log(`  垂直: ${top}..${bot}  高 ${bot - top + 1}  中心 ${(top + bot) / 2}`);
console.log(`  角落透明: ${at(2, 2)[3] === 0}`);
console.log(`  画布占比: ${(((right - left + 1) / S) * 100).toFixed(1)}%`);
