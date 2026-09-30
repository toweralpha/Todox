/**
 * 从 SQLite 文件里雕刻被删除的 task 行（稳健版）。
 *
 * # 为什么不能依赖记录头字节
 *
 * SQLite 删除行时会把 cell 的空间登记为空闲块，**空闲块自己的 4 字节头
 * （下一个空闲块偏移 + 大小）会覆写该位置原有的字节**。实测这正好覆盖了
 * 记录头的"头部长度"varint，导致 `headerStart + headerSize == 值区起点`
 * 这个等式对多数被删记录不再成立。
 *
 * # 采用的办法
 *
 * 不猜头部长度，改为**枚举序列类型区的起点**，然后要求满足：
 *   1. 从该起点依次读 varint，恰好走完到值区起点（id 的位置）
 *   2. 序列类型个数恰好等于表的列数（15）
 *   3. 按这些类型解码出的值通过语义校验：
 *        - 第 0 列是 UUID 且与定位到的文本一致
 *        - 第 3 列（time_kind）是四个合法取值之一
 *        - 若 due_at / created_at / updated_at 非空，必须是合法 RFC3339
 *        - priority ∈ 0..3，is_completed ∈ {0,1}，revision ≥ 1
 *   语义校验让"恰好解出正确记录"成为唯一的可能，避免误报。
 */

import { readFileSync, writeFileSync } from 'node:fs';

const PAGE_SIZE = 4096;
const COLUMNS = [
  'id', 'title', 'note', 'time_kind', 'due_at', 'deadline_at', 'recurrence_id',
  'priority', 'is_completed', 'completed_at', 'sort_order',
  'created_at', 'updated_at', 'deleted_at', 'revision',
];
const TIME_KINDS = new Set(['at_time', 'before_deadline', 'all_day', 'recurring']);
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
// 允许两种形态：带 T 的 RFC3339，或纯日期 YYYY-MM-DD
const STAMP_RE = /^\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}:\d{2}(\.\d+)?([+-]\d{2}:\d{2}|Z)?)?$/;

function readVarint(b, pos) {
  let v = 0n;
  for (let i = 0; i < 9; i++) {
    const x = b[pos + i];
    if (x === undefined) return null;
    if (i === 8) return [(v << 8n) | BigInt(x), 9];
    v = (v << 7n) | BigInt(x & 0x7f);
    if ((x & 0x80) === 0) return [v, i + 1];
  }
  return null;
}

function serialLength(t) {
  if (t === 0) return 0;
  if (t >= 1 && t <= 4) return t;
  if (t === 5) return 6;
  if (t === 6 || t === 7) return 8;
  if (t === 8 || t === 9) return 0;
  if (t >= 12 && t % 2 === 0) return (t - 12) / 2;
  if (t >= 13 && t % 2 === 1) return (t - 13) / 2;
  return null;
}

function decode(b, pos, t) {
  if (t === 0) return { v: null, len: 0 };
  if (t >= 1 && t <= 6) {
    let x = 0n;
    for (let i = 0; i < t; i++) x = (x << 8n) | BigInt(b[pos + i]);
    const bits = BigInt(t * 8);
    if (x >= 1n << (bits - 1n)) x -= 1n << bits;
    return { v: Number(x), len: t };
  }
  if (t === 7) return { v: b.readDoubleBE(pos), len: 8 };
  if (t === 8) return { v: 0, len: 0 };
  if (t === 9) return { v: 1, len: 0 };
  if (t >= 13 && t % 2 === 1) {
    const len = (t - 13) / 2;
    if (pos + len > b.length) return null;
    return { v: b.subarray(pos, pos + len).toString('utf8'), len };
  }
  if (t >= 12 && t % 2 === 0) {
    const len = (t - 12) / 2;
    if (pos + len > b.length) return null;
    return { v: `<blob ${len}B>`, len };
  }
  return null;
}

/** 对解出的行做语义校验 —— 这是避免误报的关键。 */
function looksLikeTaskRow(row) {
  if (!row.id || !UUID_RE.test(row.id)) return false;
  if (typeof row.title !== 'string' || row.title.length === 0) return false;
  if (!TIME_KINDS.has(row.time_kind)) return false;
  if (row.title.length > 500) return false; // schema 里有 500 字上限

  for (const k of ['due_at', 'deadline_at', 'completed_at', 'created_at', 'updated_at', 'deleted_at']) {
    const v = row[k];
    if (v !== null && (typeof v !== 'string' || !STAMP_RE.test(v))) return false;
  }
  if (row.recurrence_id !== null && !UUID_RE.test(row.recurrence_id)) return false;

  if (!Number.isInteger(row.priority) || row.priority < 0 || row.priority > 3) return false;
  if (row.is_completed !== 0 && row.is_completed !== 1) return false;
  if (!Number.isInteger(row.sort_order)) return false;
  if (!Number.isInteger(row.revision) || row.revision < 1) return false;

  // 时间基准必须与类型相符
  if (row.time_kind === 'before_deadline' && row.deadline_at === null) return false;
  if (row.time_kind !== 'before_deadline' && row.due_at === null) return false;

  return true;
}

/** 在整份文件里雕刻 task 行。 */
function carve(buf) {
  const found = new Map();
  const pageCount = Math.floor(buf.length / PAGE_SIZE);

  for (let page = 1; page <= pageCount; page++) {
    const from = (page - 1) * PAGE_SIZE;
    const to = Math.min(buf.length - 36, from + PAGE_SIZE);

    for (let pos = from; pos < to; pos++) {
      if (buf[pos + 8] !== 0x2d || buf[pos + 13] !== 0x2d) continue;
      const idText = buf.subarray(pos, pos + 36).toString('latin1');
      if (!UUID_RE.test(idText)) continue;

      // 枚举序列类型区起点
      for (let typesStart = pos - 40; typesStart < pos; typesStart++) {
        if (typesStart < 0) continue;
        let p = typesStart;
        const types = [];
        let ok = true;
        while (p < pos) {
          const tv = readVarint(buf, p);
          if (!tv) { ok = false; break; }
          if (Number(tv[0]) > 200) { ok = false; break; } // 合理的 serial type 上限
          types.push(Number(tv[0]));
          p += tv[1];
          if (types.length > COLUMNS.length) { ok = false; break; }
        }
        if (!ok || p !== pos || types.length !== COLUMNS.length) continue;

        // 解码 15 个值
        let vp = pos;
        const values = [];
        let decoded = true;
        for (const t of types) {
          const d = decode(buf, vp, t);
          if (!d) { decoded = false; break; }
          values.push(d.v);
          vp += d.len;
        }
        if (!decoded) continue;

        const row = {};
        COLUMNS.forEach((c, i) => (row[c] = values[i]));
        if (!looksLikeTaskRow(row)) continue;

        if (!found.has(row.id)) {
          found.set(row.id, { ...row, _at: `page ${page} offset ${pos}` });
        }
        break; // 已确认，换下一个候选位置
      }
    }
  }
  return [...found.values()];
}

const dbPath = process.env.APPDATA + '\\com.tower.todox\\todox.db';
const buf = readFileSync(dbPath);
const rows = carve(buf);

console.log(`从 ${dbPath} 雕刻出 ${rows.length} 条 task 记录\n`);
console.log(JSON.stringify(rows, null, 2));

writeFileSync(
  new URL('./carved.json', import.meta.url).pathname.replace(/^\//, ''),
  JSON.stringify(rows, null, 2),
  'utf8'
);
console.log(`\n已写入 carved.json`);
