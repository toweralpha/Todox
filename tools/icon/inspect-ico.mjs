// 直接解析 ICO 文件结构，列出所有帧（不依赖 .NET 的 Icon 类猜测）。
// ICO 格式：6 字节头 + 每帧 16 字节目录项 + 帧数据。

import { readFileSync } from 'node:fs';

const buf = readFileSync(process.argv[2]);

const reserved = buf.readUInt16LE(0);
const type = buf.readUInt16LE(2);
const count = buf.readUInt16LE(4);

console.log(`文件: ${process.argv[2]}`);
console.log(`  reserved=${reserved}  type=${type} (1=ICO)  帧数=${count}`);
console.log('  帧列表:');

for (let i = 0; i < count; i++) {
  const off = 6 + i * 16;
  let w = buf[off];
  let h = buf[off + 1];
  // 0 表示 256
  if (w === 0) w = 256;
  if (h === 0) h = 256;
  const colors = buf[off + 2];
  const planes = buf.readUInt16LE(off + 4);
  const bpp = buf.readUInt16LE(off + 6);
  const size = buf.readUInt32LE(off + 8);
  const dataOff = buf.readUInt32LE(off + 12);

  // 判断这一帧是 PNG 还是 BMP
  const isPng =
    buf[dataOff] === 0x89 &&
    buf[dataOff + 1] === 0x50 &&
    buf[dataOff + 2] === 0x4e &&
    buf[dataOff + 3] === 0x47;

  console.log(
    `    [${i}] ${String(w).padStart(3)}x${String(h).padEnd(3)}  bpp=${String(bpp).padStart(2)}  ` +
      `大小=${String(size).padStart(6)}  ${isPng ? 'PNG' : 'BMP'}`
  );
}
