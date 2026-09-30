/**
 * 雕刻 reminder 与 task_completion 表的被删行。
 * 复用 carve2.mjs 的方法，只是列定义与校验不同。
 */

import { readFileSync, writeFileSync } from 'node:fs';

const PAGE_SIZE = 4096;
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const STAMP_RE = /^\d{4}-\d{2}-\d{2}([T ]\d{2}:\d{2}:\d{2}(\.\d+)?([+-]\d{2}:\d{2}|Z)?)?$/;

const TABLES = {
  reminder: {
    columns: [
      'id', 'task_id', 'offset_seconds', 'snooze_until', 'is_enabled',
      'last_fired_at', 'created_at', 'updated_at', 'deleted_at', 'revision',
    ],
    check(row) {
      if (!row.id || !UUID_RE.test(row.id)) return false;
      if (!row.task_id || !UUID_RE.test(row.task_id)) return false;
      if (!Number.isInteger(row.offset_seconds)) return false;
      if (Math.abs(row.offset_seconds) > 10 * 365 * 24 * 3600) return false;
      if (row.is_enabled !== 0 && row.is_enabled !== 1) return false;
      for (const k of ['snooze_until', 'last_fired_at', 'created_at', 'updated_at', 'deleted_at']) {
        const v = row[k];
        if (v !== null && (typeof v !== 'string' || !STAMP_RE.test(v))) return false;
      }
      if (typeof row.created_at !== 'string') return false;
      if (!Number.isInteger(row.revision) || row.revision < 1) return false;
      return true;
    },
  },
  task_completion: {
    columns: [
      'id', 'task_id', 'occurrence_at', 'completed_at',
      'created_at', 'updated_at', 'deleted_at', 'revision',
    ],
    check(row) {
      if (!row.id || !UUID_RE.test(row.id)) return false;
      if (!row.task_id || !UUID_RE.test(row.task_id)) return false;
      if (typeof row.completed_at !== 'string' || !STAMP_RE.test(row.completed_at)) return false;
      for (const k of ['occurrence_at', 'created_at', 'updated_at', 'deleted_at']) {
        const v = row[k];
        if (v !== null && (typeof v !== 'string' || !STAMP_RE.test(v))) return false;
      }
      if (!Number.isInteger(row.revision) || row.revision < 1) return false;
      return true;
    },
  },
};

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

function carveTable(buf, spec) {
  const found = new Map();
  const pageCount = Math.floor(buf.length / PAGE_SIZE);

  for (let page = 1; page <= pageCount; page++) {
    const from = (page - 1) * PAGE_SIZE;
    const to = Math.min(buf.length - 36, from + PAGE_SIZE);

    for (let pos = from; pos < to; pos++) {
      if (buf[pos + 8] !== 0x2d || buf[pos + 13] !== 0x2d) continue;
      const idText = buf.subarray(pos, pos + 36).toString('latin1');
      if (!UUID_RE.test(idText)) continue;

      for (let typesStart = pos - 40; typesStart < pos; typesStart++) {
        if (typesStart < 0) continue;
        let p = typesStart;
        const types = [];
        let ok = true;
        while (p < pos) {
          const tv = readVarint(buf, p);
          if (!tv) { ok = false; break; }
          if (Number(tv[0]) > 200) { ok = false; break; }
          types.push(Number(tv[0]));
          p += tv[1];
          if (types.length > spec.columns.length) { ok = false; break; }
        }
        if (!ok || p !== pos || types.length !== spec.columns.length) continue;

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
        spec.columns.forEach((c, i) => (row[c] = values[i]));
        if (!spec.check(row)) continue;

        if (!found.has(row.id)) found.set(row.id, { ...row, _at: `page ${page} offset ${pos}` });
        break;
      }
    }
  }
  return [...found.values()];
}

const dbPath = process.env.APPDATA + '\\com.tower.todox\\todox.db';
const buf = readFileSync(dbPath);
const out = {};

for (const [name, spec] of Object.entries(TABLES)) {
  const rows = carveTable(buf, spec);
  out[name] = rows;
  console.log(`=== ${name}: 雕刻出 ${rows.length} 行 ===`);
  for (const r of rows) console.log('  ' + JSON.stringify(r));
  console.log('');
}

writeFileSync('carved-extra.json', JSON.stringify(out, null, 2), 'utf8');
console.log('已写入 carved-extra.json');
