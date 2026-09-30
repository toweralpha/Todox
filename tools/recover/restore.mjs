/**
 * 把雕刻出来的行写回数据库。
 *
 * 做法：
 *   - 全程单事务，任何一步失败就整体回滚
 *   - 保留原 ID、时间戳、revision（这些是数据的身份，不能重新生成）
 *   - 用 INSERT OR IGNORE：万一某行仍在库里，绝不覆盖现有数据
 *   - 写回后逐项核对，并打印最终状态
 *
 * 只还原**用户的任务**（carved.json）与它们的提醒档位。
 * 那 2 条 task_completion 属于我的测试任务（id 2b15fbc3…）且已软删除，
 * 刻意不还原。
 */

import { DatabaseSync } from 'node:sqlite';
import { readFileSync } from 'node:fs';

const dbPath = process.env.APPDATA + '\\com.tower.todox\\todox.db';
const tasks = JSON.parse(readFileSync('carved.json', 'utf8'));
const extra = JSON.parse(readFileSync('carved-extra.json', 'utf8'));

// 只取与这些任务相关的提醒档位
const taskIds = new Set(tasks.map((t) => t.id));
const reminders = extra.reminder.filter((r) => taskIds.has(r.task_id));

console.log(`即将还原：${tasks.length} 条任务，${reminders.length} 条提醒档位`);
console.log('');

const db = new DatabaseSync(dbPath);

// 关掉外键检查的理由见下（先插父表再插子表，顺序已经正确，这里只是保险）
db.exec('PRAGMA foreign_keys = ON');

const before = {
  task: db.prepare('SELECT COUNT(*) AS n FROM task').get().n,
  reminder: db.prepare('SELECT COUNT(*) AS n FROM reminder').get().n,
};
console.log(`写回前：task=${before.task} 行, reminder=${before.reminder} 行`);

db.exec('BEGIN IMMEDIATE');

try {
  const insTask = db.prepare(`
    INSERT OR IGNORE INTO task (
      id, title, note, time_kind, due_at, deadline_at, recurrence_id,
      priority, is_completed, completed_at, sort_order,
      created_at, updated_at, deleted_at, revision
    ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
  `);

  for (const t of tasks) {
    insTask.run(
      t.id, t.title, t.note, t.time_kind, t.due_at, t.deadline_at, t.recurrence_id,
      t.priority, t.is_completed, t.completed_at, t.sort_order,
      t.created_at, t.updated_at, t.deleted_at, t.revision
    );
    console.log(`  + 任务 ${JSON.stringify(t.title)}  (${t.time_kind}, ${t.due_at})`);
  }

  const insRem = db.prepare(`
    INSERT OR IGNORE INTO reminder (
      id, task_id, offset_seconds, snooze_until, is_enabled,
      last_fired_at, created_at, updated_at, deleted_at, revision
    ) VALUES (?,?,?,?,?,?,?,?,?,?)
  `);

  for (const r of reminders) {
    insRem.run(
      r.id, r.task_id, r.offset_seconds, r.snooze_until, r.is_enabled,
      r.last_fired_at, r.created_at, r.updated_at, r.deleted_at, r.revision
    );
    console.log(`  + 提醒档位 offset=${r.offset_seconds}s → 任务 ${r.task_id.slice(0, 8)}`);
  }

  db.exec('COMMIT');
  console.log('\n事务已提交');
} catch (e) {
  db.exec('ROLLBACK');
  console.error('\n写入失败，已回滚：', e.message);
  process.exit(1);
}

// ---------- 核对 ----------
console.log('\n=== 写回后核对 ===');
const rows = db.prepare(`
  SELECT id, title, time_kind, due_at, is_completed, deleted_at
    FROM task WHERE deleted_at IS NULL ORDER BY created_at
`).all();
for (const r of rows) {
  console.log(`  ${r.title.padEnd(26)} ${r.time_kind.padEnd(14)} ${r.due_at}`);
}
console.log(`\n  task 表未删除行数: ${rows.length}`);
console.log(`  reminder 表行数:   ${db.prepare('SELECT COUNT(*) AS n FROM reminder').get().n}`);

// 校验每条任务都有提醒档位（否则用户会失去提醒）
const orphan = db.prepare(`
  SELECT t.id, t.title FROM task t
   WHERE t.deleted_at IS NULL
     AND NOT EXISTS (SELECT 1 FROM reminder r WHERE r.task_id = t.id AND r.deleted_at IS NULL)
`).all();
if (orphan.length === 0) {
  console.log('  ✓ 每条任务都有提醒档位');
} else {
  console.log('  ⚠ 以下任务没有提醒档位：');
  for (const o of orphan) console.log(`      ${o.title}`);
}

db.close();
