# 图标生成工具

Todox 的应用图标是**用代码生成的**，不是手工画出来的位图。这样做的好处是
完全可复现、可调参、并且改动有版本记录 —— 想调粗一点笔画只需要改一个数字。

## 快速使用

```bash
cd tools/icon
node make-icon.mjs                            # 生成 icon-source.png (1024x1024) —— 约 0.5 秒

cd ../..
pnpm tauri icon tools/icon/icon-source.png    # 生成全部规格到 src-tauri/icons/
```

`pnpm tauri icon` 会生成 Windows 需要的 `.ico`（含 16/24/32/48/64/256 六个帧）、
`.png` 各尺寸，以及 Linux/macOS/iOS/Android 的格式。

> **`icon-source.png` 不纳入版本控制**（`.gitignore` 里有对应的忽略规则），
> 这是刻意的：它是生成产物，提交它会造成"改了 `make-icon.mjs` 却忘了
> 重新生成"的脱节 —— 而仓库里存着一个过期的二进制很难被发现。
> 生成只要 0.5 秒，需要时跑一下即可。
>
> 真正纳入版本控制的是 `make-icon.mjs` 里的**参数**，
> 那才是设计的唯一真相源。

## 为什么不用 sharp / Pillow / ImageMagick

图标只生成一次。为它引入一个带原生扩展的图像库会同时增加：安装体积
（几十 MB）、平台兼容风险（原生扩展在 Windows 上尤其容易出问题）、
以及供应链攻击面。而 PNG 的格式本身很简单（zlib 压缩的扫描线 + CRC 校验），
Node 内置的 `zlib` 就够了。

代价是需要自己实现抗锯齿。这里用的是**有符号距离场（SDF）**：
每个形状表达成"到形状边界的距离"，然后按距离做 1 像素宽的线性过渡。
这比超采样更快，而且形状数学完全由自己控制，不受任何渲染后端差异影响。

## 文件说明

| 文件 | 用途 |
|---|---|
| `renderer.mjs` | 极简 2D 渲染器 + PNG 编解码。SDF 形状、抗锯齿、CRC32 |
| `make-icon.mjs` | **生成最终图标**（改设计就改这个文件顶部的参数） |
| `small-sizes.mjs` | 把候选渲成 16/20/24/32/48/64px 做清晰度对比 |
| `extract-frame.mjs` | 从 `.ico` 里取出某一帧并放大，用于验证小尺寸质量 |
| `inspect-ico.mjs` | 列出 `.ico` 里所有帧的尺寸/位深/压缩方式 |
| `对比-对勾变体.png` | 5 种对勾造型在 6 种尺寸下的对比（设计过程存档） |
| `对比-笔画粗细.png` | 3 档笔画粗细在 6 种尺寸下的对比 |
| `验证-16px实际帧.png` | 从最终 `.ico` 里取出的 16px 帧（放大 10 倍） |

## 设计决策

### 为什么是"蓝色圆角方块 + 白色对勾"

1. **图标的第一职责是能被认出，不是表达个性。**
   在任务栏 16px 下，任何超过两个视觉元素的构图都会糊成一团。
   实测过打勾列表、环形进度两种更像"设计师作品"的方案，
   它们在 32px 以下都失去了可辨识度。

2. **对勾是零学习成本的符号。** 用户不需要理解一个抽象标记。

3. **squircle（超椭圆，n≈5）而不是普通圆角矩形。** 超椭圆在视觉上更饱满、
   更接近系统组件的语言；同样的视觉重量下占地面积更大，小尺寸下更醒目。

4. **笔画取中间档（0.126）。** 实测 0.112 在 24px 以上偏细、
   0.142 在 16px 下糊成色块。0.126 是两端的平衡点 —— 见
   `对比-笔画粗细.png`。

5. **渐变极克制**（`#0A84FF` → `#0A6BE0`，**只改明度不改色相**）。
   纯平色略显呆板，但强渐变会让小尺寸下的边缘发浑。

6. **对勾做了光学居中。** 机械居中时对勾会显得偏右下（视觉重量集中在
   左下的转折处），因此整体右移/上移 0.012 来补偿。
   第一版没做这个补偿，`对比-对勾变体.png` 的第一行就是那个偏心的版本。

7. **配色取自应用自身的设计系统**：`#0A84FF` 是深色模式下的强调色
   （比浅色模式的 `#007AFF` 在图标上更亮眼）。

### 尺寸与留白

底板半径 0.436（而非撑满的 0.5），使图形占画布约 **87%**。
留白是刻意的：Windows 会在图标周围再加一圈边距，
自己不留白会导致图标看起来比同排的其它图标更大、更"挤"。

## 调整设计

改 `make-icon.mjs` 顶部的这几个常量即可：

```js
const STROKE = 0.126;      // 对勾笔画粗细
const RADIUS = 0.436;      // 底板圆角半径
const OPTICAL_DX = 0.012;  // 光学居中补偿（水平）
const OPTICAL_DY = -0.012; // 光学居中补偿（垂直）
```

改完之后重新跑 `make-icon.mjs` 与 `pnpm tauri icon`，
并**务必用 `small-sizes.mjs` 检查 16px 与 24px 下的效果** ——
只在 1024px 下看是看不出小尺寸问题的。

## 一个必须知道的坑：改完图标后要清缓存

`pnpm tauri icon` 只是替换了 `src-tauri/icons/` 下的文件。
**可执行文件里嵌的图标来自 build script 生成的 Windows 资源文件**，
而 cargo 不一定会因为图标变化而重新运行它 —— 结果是：
你换了图标、重新构建、安装，但 exe 里还是旧图标。

判断方法：看 build script 的输出目录时间戳是否晚于图标文件。

```powershell
Get-ChildItem src-tauri/icons/icon.ico | Select LastWriteTime
Get-ChildItem src-tauri/target/release/build/todox-*/out | Select LastWriteTime
```

如果 out 目录的时间戳更早，就强制重建：

```powershell
cd src-tauri
cargo clean -p todox --release
cd ..
pnpm tauri build --bundles nsis
```

**另一个容易误判的点**：Windows 会按文件路径缓存图标，
所以即使 exe 里的图标已经更新，资源管理器/快捷方式可能仍显示旧的。
验证时不要相信缓存 —— 把 exe 复制成一个新文件名再提取图标，
或者直接换一台机器/新用户配置看。

提取 exe 内嵌图标可以用：

```powershell
Add-Type -AssemblyName System.Drawing
$ico = [System.Drawing.Icon]::ExtractAssociatedIcon("path\to\todox.exe")
$ico.ToBitmap().Save("out.png")
```
