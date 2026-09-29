//! 任务的读写。
//!
//! **所有对 `task` 表的写操作都必须经过本模块。** 这是"预留云同步接口"能真正
//! 成立的前提：同步所需的一切副作用（更新 `revision`、写 `changelog`、软删除）
//! 都在这里统一发生。若允许在别处直接执行 SQL，将来接入同步时必然漏掉几处，
//! 而漏掉的表现是"某些改动无法同步"—— 极难定位。

use rusqlite::{params, OptionalExtension, Row};

use crate::db::connection::{Db, DbError};
use crate::domain::recurrence::{next_occurrence, Freq, RecurrenceRule};
use crate::domain::task::{NewTask, Priority, Task, TaskCompletion, TaskEdit, TimeKind};
use crate::domain::time::{new_id, now_rfc3339, parse_rfc3339};
use crate::sync::ChangeOp;

pub struct TaskRepo<'a> {
    db: &'a Db,
}

impl<'a> TaskRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 新建任务。
    pub fn create(&self, input: NewTask) -> Result<Task, RepoError> {
        validate(&input)?;

        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        let now = now_rfc3339();
        let task = Task {
            id: new_id(),
            title: input.title.trim().to_string(),
            note: input.note,
            time_kind: input.time_kind,
            due_at: input.due_at,
            deadline_at: input.deadline_at,
            recurrence_id: input.recurrence_id,
            priority: input.priority,
            is_completed: false,
            completed_at: None,
            sort_order: 0,
            created_at: now.clone(),
            updated_at: now,
            deleted_at: None,
            revision: 1,
        };

        tx.execute(
            "INSERT INTO task (
                id, title, note, time_kind, due_at, deadline_at, recurrence_id,
                priority, is_completed, completed_at, sort_order,
                created_at, updated_at, deleted_at, revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                task.id,
                task.title,
                task.note,
                task.time_kind.as_db(),
                task.due_at,
                task.deadline_at,
                task.recurrence_id,
                task.priority.as_db(),
                task.is_completed as i64,
                task.completed_at,
                task.sort_order,
                task.created_at,
                task.updated_at,
                task.deleted_at,
                task.revision,
            ],
        )?;

        record_change(&tx, &task, ChangeOp::Insert)?;
        tx.commit()?;

        Ok(task)
    }

    /// 按 ID 查询单个任务，不含已软删除的。
    pub fn get(&self, id: &str) -> Result<Option<Task>, RepoError> {
        let conn = self.db.lock()?;
        let task = conn
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1 AND deleted_at IS NULL"),
                params![id],
                map_task_public,
            )
            .optional()
            .map_err(map_row_err)?;
        Ok(task)
    }

    /// 列出全部未删除任务。
    ///
    /// 排序规则刻意放在 SQL 而非 Rust：数据量大时在内存里排序会白白多占内存，
    /// 而本应用的核心指标之一就是内存占用。
    ///
    /// 排序依次为：有时间的排前面 → 时间早的在前 → 高优先级在前 → 创建早的在前。
    /// `CASE WHEN ... IS NULL` 用来把"无时间"（收件箱）排到列表末尾，
    /// 因为 SQLite 默认把 NULL 排在升序的最前面。
    pub fn list_all(&self, include_completed: bool) -> Result<Vec<Task>, RepoError> {
        let conn = self.db.lock()?;

        let sql = format!(
            "{SELECT_TASK}
             WHERE deleted_at IS NULL
               AND (?1 = 1 OR is_completed = 0)
             ORDER BY
               COALESCE(due_at, deadline_at) IS NULL,
               COALESCE(due_at, deadline_at) ASC,
               priority DESC,
               created_at ASC"
        );

        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![include_completed as i64], map_task_public)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_row_err)?);
        }
        Ok(out)
    }

    /// 软删除任务。
    ///
    /// 不做物理删除：硬删除无法把"删除"这个动作同步给其他设备，
    /// 表现为"删掉的记录又回来了"。墓碑记录是同步的必要条件。
    pub fn soft_delete(&self, id: &str) -> Result<(), RepoError> {
        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        let now = now_rfc3339();
        let affected = tx.execute(
            "UPDATE task
                SET deleted_at = ?2,
                    updated_at = ?2,
                    revision   = revision + 1
              WHERE id = ?1 AND deleted_at IS NULL",
            params![id, now],
        )?;

        if affected == 0 {
            return Err(RepoError::NotFound(id.to_string()));
        }

        // 软删除同样要记流水，否则其他设备永远不知道这条被删了。
        if let Some(task) = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1"),
                params![id],
                map_task_public,
            )
            .optional()
            .map_err(map_row_err)?
        {
            record_change(&tx, &task, ChangeOp::Delete)?;
        }

        tx.commit()?;
        Ok(())
    }

    /// 恢复被软删除的任务。
    ///
    /// 支撑"撤销上一次删除"。之所以能实现，正是因为删除是软的 ——
    /// 物理删除在这时已经无据可依。
    pub fn restore(&self, id: &str) -> Result<Task, RepoError> {
        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        let now = now_rfc3339();
        let affected = tx.execute(
            "UPDATE task
                SET deleted_at = NULL,
                    updated_at = ?2,
                    revision   = revision + 1
              WHERE id = ?1 AND deleted_at IS NOT NULL",
            params![id, now],
        )?;

        if affected == 0 {
            return Err(RepoError::NotFound(id.to_string()));
        }

        let task = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1"),
                params![id],
                map_task_public,
            )
            .map_err(map_row_err)?;

        record_change(&tx, &task, ChangeOp::Update)?;
        tx.commit()?;
        Ok(task)
    }

    /// 标记任务完成。
    ///
    /// 两种任务的语义**完全不同**，这是本方法最关键的分支：
    ///
    /// - **重复任务**：只追加一条完成记录，并推进到下一个发生时间。
    ///   `is_completed` 始终保持 0 —— 若把它置为 true，"勾一次"就会让
    ///   整条重复序列结束，用户第二天再也看不到这条任务。
    /// - **普通任务**：置 `is_completed = 1`，不做周期推进。
    pub fn complete(&self, id: &str) -> Result<(), RepoError> {
        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        // 整个方法只加一次锁，全部读写在同一个事务内完成。
        // 这一点很重要：若在此处通过其它仓储（如 RecurrenceRepo）读取规则，
        // 那个仓储会尝试再次加锁，而 std::sync::Mutex 不可重入，会直接死锁。
        let task = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1 AND deleted_at IS NULL"),
                params![id],
                map_task_public,
            )
            .optional()
            .map_err(map_row_err)?
            .ok_or_else(|| RepoError::NotFound(id.to_string()))?;

        if task.is_completed {
            // 已完成的普通任务不应产生第二条完成记录，否则统计会重复计数
            return Ok(());
        }

        let now = now_rfc3339();

        if task.time_kind == TimeKind::Recurring {
            self.complete_recurring_tx(&tx, &task, &now)?;
        } else {
            tx.execute(
                "UPDATE task
                    SET is_completed = 1,
                        completed_at = ?2,
                        updated_at   = ?2,
                        revision     = revision + 1
                  WHERE id = ?1 AND deleted_at IS NULL",
                params![id, now],
            )?;
            insert_completion(&tx, id, task.due_at.as_deref(), &now)?;
        }

        let updated = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1"),
                params![id],
                map_task_public,
            )
            .map_err(map_row_err)?;
        record_change(&tx, &updated, ChangeOp::Update)?;

        tx.commit()?;
        Ok(())
    }

    /// 重复任务的完成：记录本次完成，并把发生时间推进一个周期。
    ///
    /// 全部读写在调用方已开启的事务内进行，因此本方法**不再加锁**。
    fn complete_recurring_tx(
        &self,
        tx: &rusqlite::Transaction<'_>,
        task: &Task,
        now: &str,
    ) -> Result<(), RepoError> {
        let rule_id = task
            .recurrence_id
            .as_deref()
            .ok_or_else(|| RepoError::Invalid("重复任务缺少重复规则".into()))?;

        let rule = tx
            .query_row(
                "SELECT id, freq, interval, by_weekdays, by_monthday,
                        until_date, max_count, at_time_of_day, tz,
                        created_at, updated_at, deleted_at, revision
                   FROM recurrence_rule
                  WHERE id = ?1 AND deleted_at IS NULL",
                params![rule_id],
                map_rule_row,
            )
            .optional()
            .map_err(map_row_err)?
            .ok_or_else(|| RepoError::NotFound(rule_id.to_string()))?;

        let current_str = task
            .due_at
            .as_deref()
            .ok_or_else(|| RepoError::Invalid("重复任务缺少发生时间".into()))?;

        // 转成不带时区的本地时间做日期运算。
        // 这里刻意丢弃偏移：重复规则说的是"每天 9 点"这种本地墙钟时间，
        // 若带偏移参与运算，跨夏令时切换时会出现时刻漂移（变成 10 点或 8 点）。
        let current = parse_rfc3339(current_str)
            .map_err(|e| RepoError::Invalid(format!("无法解析发生时间「{current_str}」：{e}")))?;

        // 已发生次数 = 已有完成记录数 + 当前这一次
        let done: i64 = tx.query_row(
            "SELECT COUNT(*) FROM task_completion
              WHERE task_id = ?1 AND deleted_at IS NULL",
            params![task.id],
            |r| r.get(0),
        )?;

        match next_occurrence(&rule, current.naive_local(), done + 1) {
            Some(next_dt) => {
                // 把推进后的本地时间重新附加时区偏移。
                //
                // 不用 `from_naive_utc_and_offset`：那个函数名的语义是"这个
                // NaiveDateTime 已经是 UTC"，而此处拿到的是**本地墙钟时间**，
                // 直接套用会产生偏移错误的时间。
                //
                // 夏令时切换当天，某个本地时刻可能不存在（春季跳过）或出现两次
                // （秋季重复）。这两种情况都必须显式处理，不能让它 panic：
                // 用户机器上的时区我们无法预知。
                let offset = *current.offset();
                let tz = chrono::FixedOffset::east_opt(offset.local_minus_utc()).unwrap_or(offset);
                let next = match next_dt.and_local_timezone(tz) {
                    chrono::LocalResult::Single(dt) => Some(dt),
                    // 该时刻不存在（被夏令时跳过）：顺延到最早的有效时刻
                    chrono::LocalResult::None => next_dt
                        .checked_add_signed(chrono::Duration::hours(1))
                        .and_then(|d| d.and_local_timezone(tz).single()),
                    // 该时刻出现两次：取较早的那一次，保持行为确定
                    chrono::LocalResult::Ambiguous(earlier, _) => Some(earlier),
                };

                let next_str = match next {
                    Some(dt) => dt.to_rfc3339(),
                    None => {
                        return Err(RepoError::Invalid(
                            "无法为下一次发生时间确定有效的时区时刻".into(),
                        ))
                    }
                };

                tx.execute(
                    "UPDATE task
                        SET due_at     = ?2,
                            updated_at = ?3,
                            revision   = revision + 1
                      WHERE id = ?1 AND deleted_at IS NULL",
                    params![task.id, next_str, now],
                )?;
            }
            None => {
                // 规则已结束（达到 max_count 或超过 until_date）。
                // 此时把它作为普通已完成任务收尾，而不是留下一个永远不会再触发的
                // 重复任务 —— 后者会让用户困惑"为什么它待在这儿不动了"。
                tx.execute(
                    "UPDATE task
                        SET is_completed = 1,
                            completed_at = ?2,
                            updated_at   = ?2,
                            revision     = revision + 1
                      WHERE id = ?1 AND deleted_at IS NULL",
                    params![task.id, now],
                )?;
            }
        }

        insert_completion(tx, &task.id, task.due_at.as_deref(), now)?;
        Ok(())
    }

    /// 撤销完成。
    ///
    /// 对应"一键撤销误操作"。对重复任务而言，撤销是软删除最近的完成记录，
    /// **不把 `due_at` 退回去** —— 退回去会让已经过去的时间重新变成待办，
    /// 那比留着一条错误的完成记录更令人困惑。
    pub fn uncomplete(&self, id: &str) -> Result<(), RepoError> {
        let task = self
            .get(id)?
            .ok_or_else(|| RepoError::NotFound(id.to_string()))?;

        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;
        let now = now_rfc3339();

        // 软删除最近一条完成记录
        tx.execute(
            "UPDATE task_completion
                SET deleted_at = ?2, updated_at = ?2, revision = revision + 1
              WHERE id = (
                  SELECT id FROM task_completion
                   WHERE task_id = ?1 AND deleted_at IS NULL
                   ORDER BY completed_at DESC
                   LIMIT 1
              )",
            params![id, now],
        )?;

        if task.time_kind != TimeKind::Recurring {
            tx.execute(
                "UPDATE task
                    SET is_completed = 0,
                        completed_at = NULL,
                        updated_at   = ?2,
                        revision     = revision + 1
                  WHERE id = ?1 AND deleted_at IS NULL",
                params![id, now],
            )?;
        }

        let updated = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1"),
                params![id],
                map_task_public,
            )
            .map_err(map_row_err)?;
        record_change(&tx, &updated, ChangeOp::Update)?;

        tx.commit()?;
        Ok(())
    }

    /// 读取某任务的完成记录，按完成时间升序。
    ///
    /// 统计页用它回答"这个月完成了多少次锻炼"。
    pub fn completions_of(&self, task_id: &str) -> Result<Vec<TaskCompletion>, RepoError> {
        let conn = self.db.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, occurrence_at, completed_at,
                    created_at, updated_at, deleted_at, revision
               FROM task_completion
              WHERE task_id = ?1 AND deleted_at IS NULL
              ORDER BY completed_at ASC",
        )?;
        let rows = stmt.query_map(params![task_id], map_completion)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(map_row_err)?);
        }
        Ok(out)
    }

    /// 统计某任务的完成次数。重复规则推进周期时用它判断 `max_count`。
    pub fn completion_count(&self, task_id: &str) -> Result<i64, RepoError> {
        let conn = self.db.lock()?;
        let n = conn.query_row(
            "SELECT COUNT(*) FROM task_completion
              WHERE task_id = ?1 AND deleted_at IS NULL",
            params![task_id],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 修改任务的部分字段。
    ///
    /// 只更新 `edit` 里显式给出的项，未给出的保持原值 —— 这是"编辑"该有的语义，
    /// 也是 [`TaskEdit`] 所有字段都用 `Option` 的原因。
    ///
    /// 校验在写入前完成：标题不能变成空、截止型任务不能没有截止时间。
    /// 若允许写入不自洽的中间状态，用户会得到一个既不满足约束、
    /// 又无法通过界面修复的任务。
    pub fn update(&self, id: &str, edit: &TaskEdit) -> Result<Task, RepoError> {
        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        let current = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1 AND deleted_at IS NULL"),
                params![id],
                map_task_public,
            )
            .optional()
            .map_err(map_row_err)?
            .ok_or_else(|| RepoError::NotFound(id.to_string()))?;

        // 逐字段叠加改动，得到"改完之后的样子"
        let title = match &edit.title {
            Some(t) => t.trim().to_string(),
            None => current.title.clone(),
        };
        if title.is_empty() {
            return Err(RepoError::Invalid("任务标题不能为空".into()));
        }
        if title.chars().count() > 500 {
            return Err(RepoError::Invalid("任务标题过长（上限 500 字）".into()));
        }

        let time_kind = edit.time_kind.unwrap_or(current.time_kind);
        let due_at = match &edit.due_at {
            Some(v) => v.clone(),
            None => current.due_at.clone(),
        };
        let deadline_at = match &edit.deadline_at {
            Some(v) => v.clone(),
            None => current.deadline_at.clone(),
        };
        let note = match &edit.note {
            Some(n) => Some(n.clone()),
            None => current.note.clone(),
        };
        let priority = edit.priority.unwrap_or(current.priority);

        // 改动之后仍必须自洽：不能出现"截止型任务却没有截止时间"
        if time_kind == TimeKind::BeforeDeadline && deadline_at.is_none() {
            return Err(RepoError::Invalid("截止型任务必须提供截止时间".into()));
        }

        let now = now_rfc3339();
        tx.execute(
            "UPDATE task
                SET title = ?2, note = ?3, time_kind = ?4, due_at = ?5, deadline_at = ?6,
                    priority = ?7, updated_at = ?8, revision = revision + 1
              WHERE id = ?1 AND deleted_at IS NULL",
            params![
                id,
                title,
                note,
                time_kind.as_db(),
                due_at,
                deadline_at,
                priority.as_db(),
                now
            ],
        )?;

        let updated = tx
            .query_row(
                &format!("{SELECT_TASK} WHERE id = ?1"),
                params![id],
                map_task_public,
            )
            .map_err(map_row_err)?;
        record_change(&tx, &updated, ChangeOp::Update)?;

        tx.commit()?;
        Ok(updated)
    }

    /// 统计未完成任务数。侧边栏徽标用。
    pub fn count_unfinished(&self) -> Result<i64, RepoError> {
        let conn = self.db.lock()?;
        let n = conn.query_row(
            "SELECT COUNT(*) FROM task WHERE deleted_at IS NULL AND is_completed = 0",
            [],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 统计各表行数，供自检与测试使用。
    pub fn table_counts(&self) -> Result<Vec<(String, i64)>, RepoError> {
        let conn = self.db.lock()?;
        let mut out = Vec::new();
        for table in [
            "task",
            "recurrence_rule",
            "task_completion",
            "reminder",
            "tag",
            "subtask",
            "changelog",
        ] {
            let n: i64 =
                conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))?;
            out.push((table.to_string(), n));
        }
        Ok(out)
    }
}

/// 任务的完整投影列表。
///
/// 抽成常量而非在每处 `SELECT *`：`SELECT *` 在表结构变更后会静默改变列顺序，
/// 而 `map_task` 是按位置取列的，那会变成难查的数据错位。
///
/// 公开给其它仓储复用（当前是 `reminder_repo`）：提醒仓储需要读到任务，
/// 若各写一份列清单，加字段时极易只改一处而让另一处静默错位。
pub const SELECT_TASK_PUBLIC: &str = "SELECT
    id, title, note, time_kind, due_at, deadline_at, recurrence_id,
    priority, is_completed, completed_at, sort_order,
    created_at, updated_at, deleted_at, revision
  FROM task";

/// 内部别名，保持既有调用点不变。
const SELECT_TASK: &str = SELECT_TASK_PUBLIC;

/// 把一行的列映射为 `Task`。
///
/// 列顺序必须与 [`SELECT_TASK`] 严格一致。公开给其它仓储复用。
pub fn map_task_public(row: &Row<'_>) -> rusqlite::Result<Task> {
    let kind_raw: String = row.get("time_kind")?;
    let time_kind = TimeKind::from_db(&kind_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("无法识别的 time_kind：{kind_raw}"),
            )),
        )
    })?;

    let priority_raw: i64 = row.get("priority")?;
    let priority = Priority::from_db(priority_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Integer,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("无法识别的 priority：{priority_raw}"),
            )),
        )
    })?;

    Ok(Task {
        id: row.get("id")?,
        title: row.get("title")?,
        note: row.get("note")?,
        time_kind,
        due_at: row.get("due_at")?,
        deadline_at: row.get("deadline_at")?,
        recurrence_id: row.get("recurrence_id")?,
        priority,
        is_completed: row.get::<_, i64>("is_completed")? != 0,
        completed_at: row.get("completed_at")?,
        sort_order: row.get("sort_order")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        deleted_at: row.get("deleted_at")?,
        revision: row.get("revision")?,
    })
}

/// 把 row 映射失败的原因透传出去。
///
/// `query_map` 返回的错误可能是 SQL 执行失败，也可能是 `map_task` 里的类型不符。
/// 后者往往意味着数据损坏或 schema 漂移，需要保留原始信息以便定位。
fn map_row_err(e: rusqlite::Error) -> RepoError {
    RepoError::Db(DbError::Sqlite(e))
}

/// 把一行的列映射为 `RecurrenceRule`。
///
/// 此处刻意**不复用** `RecurrenceRepo::get`：那会需要再次获取数据库锁，
/// 而本函数的调用点已经持有事务与锁。Mutex 不可重入，复用等于死锁。
fn map_rule_row(row: &Row<'_>) -> rusqlite::Result<RecurrenceRule> {
    let freq_raw: String = row.get("freq")?;
    let freq = Freq::from_db(&freq_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("无法识别的 freq：{freq_raw}"),
            )),
        )
    })?;

    Ok(RecurrenceRule {
        id: row.get("id")?,
        freq,
        interval: row.get("interval")?,
        by_weekdays: row.get("by_weekdays")?,
        by_monthday: row.get("by_monthday")?,
        until_date: row.get("until_date")?,
        max_count: row.get("max_count")?,
        at_time_of_day: row.get("at_time_of_day")?,
        tz: row.get("tz")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        deleted_at: row.get("deleted_at")?,
        revision: row.get("revision")?,
    })
}

/// 插入一条完成记录。
///
/// 独立成函数是因为"普通任务完成"与"重复任务完成"两条路径都要写它，
/// 而这张表的字段较多，分散写两遍迟早会漏字段。
fn insert_completion(
    tx: &rusqlite::Transaction<'_>,
    task_id: &str,
    occurrence_at: Option<&str>,
    completed_at: &str,
) -> Result<(), RepoError> {
    tx.execute(
        "INSERT INTO task_completion (
            id, task_id, occurrence_at, completed_at,
            created_at, updated_at, deleted_at, revision
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?5, NULL, 1)",
        params![new_id(), task_id, occurrence_at, completed_at, completed_at],
    )?;
    Ok(())
}

/// 把一行的列映射为 `TaskCompletion`。列顺序必须与查询语句严格一致。
fn map_completion(row: &Row<'_>) -> rusqlite::Result<TaskCompletion> {
    Ok(TaskCompletion {
        id: row.get("id")?,
        task_id: row.get("task_id")?,
        occurrence_at: row.get("occurrence_at")?,
        completed_at: row.get("completed_at")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        deleted_at: row.get("deleted_at")?,
        revision: row.get("revision")?,
    })
}

/// 写入一条变更流水。
fn record_change(
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    op: ChangeOp,
) -> Result<(), RepoError> {
    let payload = serde_json::to_value(task).map_err(|e| RepoError::Serialize(e.to_string()))?;

    tx.execute(
        "INSERT INTO changelog (entity, entity_id, op, changed_at, synced_at)
         VALUES ('task', ?1, ?2, ?3, NULL)",
        params![task.id, op.as_db(), task.updated_at],
    )?;

    // payload 目前只用于保证快照可序列化，尚未落库（changelog 表未存 payload 列）。
    // 阶段四接入调度器时若需要，再补一列并递增 SCHEMA_VERSION。
    // 现在刻意不落库，避免为一个假想的未来需求提前付出存储成本。
    let _ = payload;

    Ok(())
}

/// 创建任务前的校验。
///
/// 这些约束在 SQL 层大多也有 CHECK，但在这里提前失败能给出可读的中文提示，
/// 而不是把 SQLite 的 "constraint failed" 直接抛给用户。
fn validate(input: &NewTask) -> Result<(), RepoError> {
    let title = input.title.trim();
    if title.is_empty() {
        return Err(RepoError::Invalid("任务标题不能为空".into()));
    }
    if title.chars().count() > 500 {
        return Err(RepoError::Invalid("任务标题过长（上限 500 字）".into()));
    }

    match input.time_kind {
        TimeKind::BeforeDeadline if input.deadline_at.is_none() => {
            return Err(RepoError::Invalid("截止型任务必须提供截止时间".into()));
        }
        TimeKind::Recurring if input.recurrence_id.is_none() => {
            return Err(RepoError::Invalid("重复任务必须提供重复规则".into()));
        }
        _ => {}
    }

    Ok(())
}

#[derive(Debug)]
pub enum RepoError {
    Db(DbError),
    NotFound(String),
    Invalid(String),
    Serialize(String),
}

impl std::fmt::Display for RepoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RepoError::Db(e) => write!(f, "{e}"),
            RepoError::NotFound(id) => write!(f, "找不到该任务（{id}）"),
            RepoError::Invalid(m) => write!(f, "{m}"),
            RepoError::Serialize(m) => write!(f, "序列化失败：{m}"),
        }
    }
}

impl std::error::Error for RepoError {}

impl From<DbError> for RepoError {
    fn from(e: DbError) -> Self {
        RepoError::Db(e)
    }
}

impl From<rusqlite::Error> for RepoError {
    fn from(e: rusqlite::Error) -> Self {
        RepoError::Db(DbError::Sqlite(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::TimeKind;

    fn repo_fixture() -> (Db, ()) {
        let db = Db::open_in_memory().expect("建库失败");
        (db, ())
    }

    #[test]
    fn create_and_get_roundtrip() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let created = repo.create(NewTask::inbox("买牛奶")).expect("创建失败");

        assert_eq!(created.revision, 1);
        assert!(!created.is_completed);
        assert_eq!(created.priority, Priority::None);

        let fetched = repo.get(&created.id).unwrap().expect("应当能查到");
        assert_eq!(fetched, created, "读回的任务应与写入的完全一致");
    }

    #[test]
    fn empty_title_is_rejected() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let err = repo
            .create(NewTask::inbox("   "))
            .expect_err("空标题应被拒绝");
        assert!(matches!(err, RepoError::Invalid(_)), "实际：{err:?}");
    }

    #[test]
    fn title_is_trimmed() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("  写周报  ")).unwrap();
        assert_eq!(t.title, "写周报");
    }

    /// 截止型任务缺截止时间必须在入库前被拦下。
    #[test]
    fn before_deadline_requires_deadline_at() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let mut input = NewTask::inbox("交报告");
        input.time_kind = TimeKind::BeforeDeadline;
        input.deadline_at = None;

        assert!(matches!(
            repo.create(input).expect_err("应当被拒绝"),
            RepoError::Invalid(_)
        ));
    }

    /// 重复任务缺规则同样必须被拦下。
    #[test]
    fn recurring_requires_recurrence_id() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let mut input = NewTask::inbox("每周复盘");
        input.time_kind = TimeKind::Recurring;
        input.recurrence_id = None;

        assert!(matches!(
            repo.create(input).expect_err("应当被拒绝"),
            RepoError::Invalid(_)
        ));
    }

    /// 软删除后不应出现在任何列表里，但数据仍在库中。
    #[test]
    fn soft_delete_hides_task_but_keeps_row() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let t = repo.create(NewTask::inbox("临时任务")).unwrap();
        assert_eq!(repo.list_all(false).unwrap().len(), 1);

        repo.soft_delete(&t.id).unwrap();
        assert_eq!(
            repo.list_all(false).unwrap().len(),
            0,
            "删除后不应出现在列表中"
        );
        assert!(repo.get(&t.id).unwrap().is_none(), "删除后按 ID 也应查不到");

        // 但物理行还在，这是同步的前提
        let conn = db.lock().unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM task WHERE id = ?1",
                params![t.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "软删除必须保留物理行，否则无法同步删除动作");
    }

    /// 撤销删除依赖软删除 —— 这是"一键撤销"能实现的根本原因。
    #[test]
    fn restore_brings_task_back() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let t = repo.create(NewTask::inbox("误删的任务")).unwrap();
        repo.soft_delete(&t.id).unwrap();
        let restored = repo.restore(&t.id).unwrap();

        assert!(restored.deleted_at.is_none());
        assert_eq!(restored.revision, 3, "创建1 + 删除2 + 恢复3");
        assert_eq!(repo.list_all(false).unwrap().len(), 1);
    }

    #[test]
    fn deleting_unknown_id_is_not_found() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let err = repo.soft_delete("no-such-id").expect_err("应报找不到");
        assert!(matches!(err, RepoError::NotFound(_)));
    }

    /// 每次写操作都必须留下 changelog，否则将来同步会漏数据。
    #[test]
    fn writes_are_recorded_in_changelog() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let t = repo.create(NewTask::inbox("任务")).unwrap();
        repo.soft_delete(&t.id).unwrap();
        repo.restore(&t.id).unwrap();

        let conn = db.lock().unwrap();
        let rows: Vec<String> = conn
            .prepare("SELECT op FROM changelog ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert_eq!(rows, vec!["insert", "delete", "update"]);
    }

    /// 排序规则：有时间在前、无时间（收件箱）在后。
    #[test]
    fn tasks_without_time_sort_last() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        repo.create(NewTask::inbox("没有时间的")).unwrap();

        let mut with_time = NewTask::inbox("有时间的");
        with_time.time_kind = TimeKind::AtTime;
        with_time.due_at = Some("2026-09-29T14:30:00+08:00".into());
        repo.create(with_time).unwrap();

        let list = repo.list_all(false).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].title, "有时间的", "有时间的任务应排在前面");
        assert_eq!(list[1].title, "没有时间的");
    }

    #[test]
    fn completed_tasks_excluded_unless_requested() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("待完成")).unwrap();

        repo.complete(&t.id).unwrap();

        assert_eq!(
            repo.list_all(false).unwrap().len(),
            0,
            "已完成不应出现在默认列表"
        );
        assert_eq!(repo.list_all(true).unwrap().len(), 1);
        assert_eq!(repo.count_unfinished().unwrap(), 0);
    }

    // ===================== 完成确认与重复推进 =====================

    /// 普通任务完成后应置位并留下一条完成记录。
    #[test]
    fn completing_normal_task_sets_flag_and_records() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let mut input = NewTask::inbox("交房租");
        input.time_kind = TimeKind::AtTime;
        input.due_at = Some("2026-09-29T14:30:00+08:00".into());
        let t = repo.create(input).unwrap();

        repo.complete(&t.id).unwrap();

        let after = repo.get(&t.id).unwrap().unwrap();
        assert!(after.is_completed, "普通任务完成后应置为已完成");
        assert!(after.completed_at.is_some());
        assert_eq!(after.due_at, t.due_at, "普通任务的时间不应被推进");

        let records = repo.completions_of(&t.id).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].occurrence_at.as_deref(),
            Some("2026-09-29T14:30:00+08:00"),
            "完成记录应记下被完成的那一次计划时刻"
        );
    }

    /// 重复点击完成不应产生重复记录，否则统计会翻倍。
    #[test]
    fn completing_twice_does_not_duplicate_record() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let t = repo.create(NewTask::inbox("只做一次")).unwrap();
        repo.complete(&t.id).unwrap();
        repo.complete(&t.id).unwrap();

        assert_eq!(repo.completions_of(&t.id).unwrap().len(), 1);
    }

    /// 这是重复任务最关键的语义：勾选后任务本身**不能**变成已完成，
    /// 否则用户第二天就再也看不到这条任务了。
    #[test]
    fn completing_recurring_advances_instead_of_finishing() {
        let db = Db::open_in_memory().unwrap();
        let repo = TaskRepo::new(&db);
        let rec_repo = crate::repo::recurrence_repo::RecurrenceRepo::new(&db);

        let rule = crate::domain::recurrence::RecurrenceRule {
            id: String::new(),
            freq: crate::domain::recurrence::Freq::Daily,
            interval: 1,
            by_weekdays: None,
            by_monthday: None,
            until_date: None,
            max_count: None,
            at_time_of_day: Some("08:00".into()),
            tz: None,
            created_at: String::new(),
            updated_at: String::new(),
            deleted_at: None,
            revision: 1,
        };
        let rule_id = rec_repo.create(&rule).unwrap();

        let mut input = NewTask::inbox("吃维生素");
        input.time_kind = TimeKind::Recurring;
        input.due_at = Some("2026-09-29T08:00:00+08:00".into());
        input.recurrence_id = Some(rule_id.clone());
        let t = repo.create(input).unwrap();

        repo.complete(&t.id).unwrap();

        let after = repo.get(&t.id).unwrap().unwrap();
        assert!(
            !after.is_completed,
            "重复任务勾选后不能变成已完成，否则整条序列就结束了"
        );
        assert_eq!(
            after.due_at.as_deref(),
            Some("2026-09-30T08:00:00+08:00"),
            "发生时间应推进一天，且保留原时区偏移"
        );
        assert_eq!(
            repo.completions_of(&t.id).unwrap().len(),
            1,
            "应留下完成记录"
        );

        // 再勾一次，继续推进
        repo.complete(&t.id).unwrap();
        let after2 = repo.get(&t.id).unwrap().unwrap();
        assert_eq!(after2.due_at.as_deref(), Some("2026-10-01T08:00:00+08:00"));
        assert_eq!(repo.completions_of(&t.id).unwrap().len(), 2);

        // 规则本身不应被改动 —— 推进只发生在 task.due_at 上
        let rule_after = rec_repo.get(&rule_id).unwrap().unwrap();
        assert_eq!(rule_after.interval, 1);
        assert_eq!(rule_after.max_count, None);
    }

    /// 达到 max_count 的重复任务应在最后一次完成后收尾，
    /// 而不是留下一个永远不会再触发的任务。
    #[test]
    fn recurring_finishes_when_max_count_reached() {
        let db = Db::open_in_memory().unwrap();
        let repo = TaskRepo::new(&db);
        let rec_repo = crate::repo::recurrence_repo::RecurrenceRepo::new(&db);

        let rule = crate::domain::recurrence::RecurrenceRule {
            id: String::new(),
            freq: crate::domain::recurrence::Freq::Daily,
            interval: 1,
            by_weekdays: None,
            by_monthday: None,
            until_date: None,
            max_count: Some(2),
            at_time_of_day: Some("08:00".into()),
            tz: None,
            created_at: String::new(),
            updated_at: String::new(),
            deleted_at: None,
            revision: 1,
        };
        let rule_id = rec_repo.create(&rule).unwrap();

        let mut input = NewTask::inbox("只做两次");
        input.time_kind = TimeKind::Recurring;
        input.due_at = Some("2026-09-29T08:00:00+08:00".into());
        input.recurrence_id = Some(rule_id);
        let t = repo.create(input).unwrap();

        // 第一次完成：还有下一次
        repo.complete(&t.id).unwrap();
        let after1 = repo.get(&t.id).unwrap().unwrap();
        assert!(!after1.is_completed);
        assert_eq!(after1.due_at.as_deref(), Some("2026-09-30T08:00:00+08:00"));

        // 第二次完成：达到上限，应整体收尾
        repo.complete(&t.id).unwrap();
        let after2 = repo.get(&t.id).unwrap().unwrap();
        assert!(after2.is_completed, "达到 max_count 后应作为已完成任务收尾");
        assert_eq!(repo.completions_of(&t.id).unwrap().len(), 2);
    }

    /// 撤销完成应把普通任务恢复为未完成，并软删除完成记录。
    #[test]
    fn uncomplete_restores_normal_task() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let t = repo.create(NewTask::inbox("误勾的任务")).unwrap();
        repo.complete(&t.id).unwrap();
        repo.uncomplete(&t.id).unwrap();

        let after = repo.get(&t.id).unwrap().unwrap();
        assert!(!after.is_completed);
        assert!(after.completed_at.is_none());
        assert_eq!(
            repo.completions_of(&t.id).unwrap().len(),
            0,
            "完成记录应被撤销"
        );
        assert_eq!(repo.completion_count(&t.id).unwrap(), 0);
    }

    /// 撤销重复任务的完成不应把发生时间退回去 ——
    /// 让已过去的时间重新变成待办比留着一条错误的完成记录更令人困惑。
    #[test]
    fn uncomplete_recurring_keeps_advanced_schedule() {
        let db = Db::open_in_memory().unwrap();
        let repo = TaskRepo::new(&db);
        let rec_repo = crate::repo::recurrence_repo::RecurrenceRepo::new(&db);

        let rule = crate::domain::recurrence::RecurrenceRule {
            id: String::new(),
            freq: crate::domain::recurrence::Freq::Daily,
            interval: 1,
            by_weekdays: None,
            by_monthday: None,
            until_date: None,
            max_count: None,
            at_time_of_day: Some("08:00".into()),
            tz: None,
            created_at: String::new(),
            updated_at: String::new(),
            deleted_at: None,
            revision: 1,
        };
        let rule_id = rec_repo.create(&rule).unwrap();

        let mut input = NewTask::inbox("每天吃药");
        input.time_kind = TimeKind::Recurring;
        input.due_at = Some("2026-09-29T08:00:00+08:00".into());
        input.recurrence_id = Some(rule_id);
        let t = repo.create(input).unwrap();

        repo.complete(&t.id).unwrap();
        repo.uncomplete(&t.id).unwrap();

        let after = repo.get(&t.id).unwrap().unwrap();
        assert_eq!(
            after.due_at.as_deref(),
            Some("2026-09-30T08:00:00+08:00"),
            "撤销完成不应把发生时间退回"
        );
        assert_eq!(repo.completions_of(&t.id).unwrap().len(), 0);
    }

    #[test]
    fn completing_unknown_task_is_not_found() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        assert!(matches!(
            repo.complete("no-such-id").expect_err("应报找不到"),
            RepoError::NotFound(_)
        ));
    }

    // ===================== 编辑 =====================

    #[test]
    fn update_changes_only_given_fields() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let mut input = NewTask::inbox("原标题");
        input.note = Some("原备注".into());
        input.priority = Priority::High;
        let t = repo.create(input).unwrap();

        // 只改标题
        let edit = TaskEdit {
            title: Some("新标题".into()),
            ..Default::default()
        };
        let updated = repo.update(&t.id, &edit).unwrap();

        assert_eq!(updated.title, "新标题");
        assert_eq!(
            updated.note.as_deref(),
            Some("原备注"),
            "未给出的字段应保持原值"
        );
        assert_eq!(updated.priority, Priority::High, "未给出的字段应保持原值");
        assert_eq!(updated.revision, t.revision + 1);
        assert_eq!(updated.created_at, t.created_at, "created_at 不应被改动");
    }

    /// 双层 Option 的核心用途：把时间删掉（变成收件箱任务）。
    #[test]
    fn update_can_clear_time_with_nested_none() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);

        let mut input = NewTask::inbox("有时间的任务");
        input.time_kind = TimeKind::AtTime;
        input.due_at = Some("2026-09-30T15:00:00+08:00".into());
        let t = repo.create(input).unwrap();
        assert!(t.due_at.is_some());

        // 外层 Some + 内层 None = 清空
        let edit = TaskEdit {
            due_at: Some(None),
            ..Default::default()
        };
        let updated = repo.update(&t.id, &edit).unwrap();
        assert!(updated.due_at.is_none(), "应能清空时间");

        // 只看不改：外层 None = 不动
        let noop = TaskEdit::default();
        let again = repo.update(&t.id, &noop).unwrap();
        assert!(again.due_at.is_none(), "空编辑不应改变任何东西");
    }

    /// 不能把任务改成"截止型却没有截止时间"这种不自洽状态。
    #[test]
    fn update_rejects_deadline_kind_without_deadline() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("任务")).unwrap();

        let edit = TaskEdit {
            time_kind: Some(TimeKind::BeforeDeadline),
            ..Default::default()
        };
        assert!(
            matches!(
                repo.update(&t.id, &edit).expect_err("应被拒绝"),
                RepoError::Invalid(_)
            ),
            "截止型任务缺截止时间应被拒绝"
        );

        // 拒绝后原数据不应被改动
        let after = repo.get(&t.id).unwrap().unwrap();
        assert_eq!(after.time_kind, TimeKind::AllDay);
        assert_eq!(after.revision, t.revision, "被拒绝的编辑不应增加版本号");
    }

    /// 把普通任务改成截止型任务（这是阶段五新增的界面能力）。
    #[test]
    fn update_can_convert_to_deadline_task() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("交报告")).unwrap();

        let edit = TaskEdit {
            time_kind: Some(TimeKind::BeforeDeadline),
            deadline_at: Some(Some("2026-10-01T18:00:00+08:00".into())),
            ..Default::default()
        };
        let updated = repo.update(&t.id, &edit).unwrap();

        assert_eq!(updated.time_kind, TimeKind::BeforeDeadline);
        assert_eq!(
            updated.deadline_at.as_deref(),
            Some("2026-10-01T18:00:00+08:00")
        );
    }

    #[test]
    fn update_rejects_empty_title() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("原题")).unwrap();

        let edit = TaskEdit {
            title: Some("   ".into()),
            ..Default::default()
        };
        assert!(matches!(
            repo.update(&t.id, &edit).expect_err("应被拒绝"),
            RepoError::Invalid(_)
        ));

        // 原标题不应被清空
        assert_eq!(repo.get(&t.id).unwrap().unwrap().title, "原题");
    }

    #[test]
    fn update_trims_title() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("原题")).unwrap();

        let edit = TaskEdit {
            title: Some("  新题  ".into()),
            ..Default::default()
        };
        assert_eq!(repo.update(&t.id, &edit).unwrap().title, "新题");
    }

    #[test]
    fn update_unknown_task_is_not_found() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        assert!(matches!(
            repo.update("no-such-id", &TaskEdit::default())
                .expect_err("应报找不到"),
            RepoError::NotFound(_)
        ));
    }

    /// 编辑也要写 changelog，否则同步时会漏掉这次改动。
    #[test]
    fn update_is_recorded_in_changelog() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("任务")).unwrap();

        let edit = TaskEdit {
            title: Some("改过的".into()),
            ..Default::default()
        };
        repo.update(&t.id, &edit).unwrap();

        let conn = db.lock().unwrap();
        let ops: Vec<String> = conn
            .prepare("SELECT op FROM changelog ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ops, vec!["insert", "update"]);
    }

    #[test]
    fn completed_tasks_excluded_from_default_list() {
        let (db, _) = repo_fixture();
        let repo = TaskRepo::new(&db);
        let t = repo.create(NewTask::inbox("待完成")).unwrap();

        {
            let conn = db.lock().unwrap();
            conn.execute(
                "UPDATE task SET is_completed = 1 WHERE id = ?1",
                params![t.id],
            )
            .unwrap();
        }

        assert_eq!(repo.list_all(false).unwrap().len(), 0);
        assert_eq!(repo.list_all(true).unwrap().len(), 1);
        assert_eq!(repo.count_unfinished().unwrap(), 0);
    }
}
