//! 提醒的读写。
//!
//! 提醒档位在任务创建时按设置里的默认策略落库（"物化"），而不是每次都由
//! 时间类型临时推导。理由：阶段五要给用户逐条编辑提醒的能力（"这个任务
//! 不用提前一天提醒"），那需要一个可持久化的载体。
//!
//! 与调度器"不物化下次触发时刻"的取舍并不矛盾，因为两者物化的东西性质不同：
//!
//! - 物化的是**档位定义**（相对偏移），它不随时间流逝而失效；
//! - 不物化的是**绝对触发时刻**，它会因休眠、时钟调整、进程重启而失真。
//!
//! 因此前者可以安全地存进数据库，后者必须每次重新推导。

use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::db::connection::{Db, DbError};
use crate::domain::task::{Task, TimeKind};
use crate::domain::time::{new_id, now_rfc3339};
use crate::repo::settings_repo::{AppSettings, SettingsRepo};
use crate::repo::task_repo::{map_task_public, RepoError, SELECT_TASK_PUBLIC};

/// 一条提醒档位（数据库形态）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReminderTierRow {
    pub id: String,
    pub task_id: String,
    pub offset_seconds: i64,
    /// 用户一次性"稍后提醒"产生的绝对时刻。存在时优先于 offset。
    pub snooze_until: Option<String>,
    pub is_enabled: bool,
    /// 最近一次实际弹出的时刻。防重复提醒与错过补发都依赖它。
    pub last_fired_at: Option<String>,
}

/// 一条错过记录。
///
/// 需要可序列化：它要跨 Tauri 边界传给前端，用于显示"你错过了 X 个提醒"。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MissedReminderRow {
    pub id: String,
    pub task_id: String,
    pub task_title: String,
    pub scheduled_at: String,
    pub detected_at: String,
    pub is_acknowledged: bool,
}

pub struct ReminderRepo<'a> {
    db: &'a Db,
}

impl<'a> ReminderRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 按任务的时间类型，从设置中取默认档位并落库。
    ///
    /// 幂等：已有档位的任务不会被重复插入。这让它可以在任务创建流程里
    /// 无脑调用，不必先查一遍。
    pub fn create_defaults(&self, task: &Task) -> Result<(), RepoError> {
        let settings = SettingsRepo::new(self.db).load()?;
        self.create_defaults_with(task, &settings)
    }

    /// [`Self::create_defaults`] 的实现，设置由调用方注入（便于测试）。
    pub fn create_defaults_with(
        &self,
        task: &Task,
        settings: &AppSettings,
    ) -> Result<(), RepoError> {
        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        // 已存在则不重复插入
        let existing: i64 = tx.query_row(
            "SELECT COUNT(*) FROM reminder WHERE task_id = ?1 AND deleted_at IS NULL",
            params![task.id],
            |r| r.get(0),
        )?;
        if existing > 0 {
            return Ok(());
        }

        insert_tiers(&tx, task, settings)?;
        tx.commit()?;
        Ok(())
    }

    /// 清掉某任务的全部档位，再按当前设置重建。
    ///
    /// 用在任务的时间类型发生改变时：时间点任务只有"到点提醒"，而截止型任务
    /// 有"前 1 天 / 前 1 小时 / 到点"三级。类型变了却不重建档位，用户会看到
    /// "我把任务改成截止型了，但提醒还是只有一条"。
    ///
    /// # 为什么软删与重建必须在同一个事务里
    ///
    /// 早期实现是"先在自动提交的语句里软删，再在另一个事务里创建"。
    /// 两步之间只要第二步失败（外部工具持有写锁导致 `SQLITE_BUSY`、
    /// 磁盘写满、连接异常），结果就是**旧档位已删、新档位没写** ——
    /// 该任务从此没有任何提醒，而唯一的痕迹只是一行 stderr。
    /// 用户只是改了一次时间类型，却静默失去了提醒能力。
    ///
    /// 放进同一事务后，任一步失败都整体回滚，最坏情况是"档位没变"。
    pub fn reset_defaults(&self, task: &Task) -> Result<(), RepoError> {
        let settings = SettingsRepo::new(self.db).load()?;

        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;

        let now = now_rfc3339();
        // 软删除旧档位而不是物理删除：与其它表保持一致的同步语义
        tx.execute(
            "UPDATE reminder
                SET deleted_at = ?2, updated_at = ?2, revision = revision + 1
              WHERE task_id = ?1 AND deleted_at IS NULL",
            params![task.id, now],
        )?;

        // 注意这里**不**调用 create_defaults：那条路径会再次 `lock()`，
        // 而 std::sync::Mutex 不可重入 —— 在已经持有锁的情况下调用会直接死锁。
        // 因此把插入逻辑抽成自由函数，接收已有的 `&Transaction`。
        insert_tiers(&tx, task, &settings)?;

        tx.commit()?;
        Ok(())
    }

    /// 读取某任务的提醒档位。
    pub fn tiers_of(&self, task_id: &str) -> Result<Vec<ReminderTierRow>, RepoError> {
        let conn = self.db.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, offset_seconds, snooze_until, is_enabled, last_fired_at
               FROM reminder
              WHERE task_id = ?1 AND deleted_at IS NULL
              ORDER BY offset_seconds ASC",
        )?;
        let rows = stmt.query_map(params![task_id], map_tier)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 取出所有"未完成且带提醒"的任务及其档位。
    ///
    /// 这是调度器每次重算时的唯一数据来源。刻意做成一次性批量查询而不是
    /// 每个任务查一次：任务数量可能有几百个，逐个查询会变成 N+1 问题。
    pub fn tasks_with_reminders(&self) -> Result<Vec<(Task, Vec<ReminderTierRow>)>, RepoError> {
        let conn = self.db.lock()?;

        // 一次取出所有任务
        let sql = format!(
            "{SELECT_TASK_PUBLIC}
             WHERE deleted_at IS NULL AND is_completed = 0
             ORDER BY id"
        );
        let mut stmt = conn.prepare(&sql)?;
        let tasks: Vec<Task> = stmt
            .query_map([], map_task_public)?
            .collect::<Result<_, _>>()?;
        drop(stmt);

        // 一次取出所有档位，再在内存里按 task_id 归组
        let mut stmt2 = conn.prepare(
            "SELECT id, task_id, offset_seconds, snooze_until, is_enabled, last_fired_at
               FROM reminder
              WHERE deleted_at IS NULL
              ORDER BY task_id, offset_seconds",
        )?;
        let all_tiers: Vec<ReminderTierRow> =
            stmt2.query_map([], map_tier)?.collect::<Result<_, _>>()?;
        drop(stmt2);

        let mut grouped: std::collections::HashMap<String, Vec<ReminderTierRow>> =
            std::collections::HashMap::new();
        for t in all_tiers {
            grouped.entry(t.task_id.clone()).or_default().push(t);
        }

        Ok(tasks
            .into_iter()
            .map(|task| {
                let tiers = grouped.remove(&task.id).unwrap_or_default();
                (task, tiers)
            })
            .collect())
    }

    /// 标记某个档位已触发（或已被补发）。
    pub fn mark_fired(&self, task_id: &str, offset_seconds: i64) -> Result<(), RepoError> {
        let conn = self.db.lock()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE reminder
                SET last_fired_at = ?3,
                    updated_at    = ?3,
                    revision      = revision + 1
              WHERE task_id = ?1 AND offset_seconds = ?2 AND deleted_at IS NULL",
            params![task_id, offset_seconds, now],
        )?;
        Ok(())
    }

    /// 设置"稍后提醒"的绝对时刻。
    ///
    /// 写入 `snooze_until` 后，调度器会把它作为该档位的新触发时刻。
    pub fn snooze(&self, task_id: &str, offset_seconds: i64, until: &str) -> Result<(), RepoError> {
        let conn = self.db.lock()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE reminder
                SET snooze_until = ?3,
                    updated_at   = ?4,
                    revision     = revision + 1
              WHERE task_id = ?1 AND offset_seconds = ?2 AND deleted_at IS NULL",
            params![task_id, offset_seconds, until, now],
        )?;
        Ok(())
    }

    /// 把**最近触发过的那条**提醒推迟一段时间（取设置的 `snooze_minutes`）。
    ///
    /// 用于通知里的「稍后提醒」按钮。
    ///
    /// # 为什么用"最近触发"而不是指定任务
    ///
    /// toast 的按钮只能携带我们写死的参数（`todox://snooze`），无法把任务 ID
    /// 一起传回来。但用户点这个按钮时，刚弹出的那条通知就是最近触发的那条，
    /// 因此"最近一次触发"是可靠且有意义的近似。
    ///
    /// 返回实际推迟的分钟数；`Ok(None)` 表示没有找到可推迟的提醒
    /// （例如它在用户点击之前已被删除）。
    pub fn snooze_most_recent(&self) -> Result<Option<i64>, RepoError> {
        let settings = SettingsRepo::new(self.db).load()?;
        let minutes = settings.snooze_minutes.max(1);

        let conn = self.db.lock()?;

        // 找最近触发的档位。`last_fired_at` 是 RFC3339，
        // 可按字符串排序比较 —— 这正是选择这种格式存储的好处之一。
        let target: Option<(String, i64)> = conn
            .query_row(
                "SELECT task_id, offset_seconds FROM reminder
                  WHERE last_fired_at IS NOT NULL AND deleted_at IS NULL
                  ORDER BY last_fired_at DESC
                  LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(|e| RepoError::Db(DbError::Sqlite(e)))?;

        let Some((task_id, offset_seconds)) = target else {
            return Ok(None);
        };

        // 推迟到「现在 + N 分钟」。
        //
        // 用 `SecondsFormat::Secs` 而不是裸 `to_rfc3339()`：后者会带上纳秒
        // （`...T21:56:51.796191400+08:00`）。纳秒对提醒毫无意义，而且会破坏
        // "所有时间戳精度一致"这一点 —— 一旦某处需要按字符串比较或排序
        // 两种精度混在一起的时间戳，就会出现难以察觉的错误顺序。
        let until = (chrono::Local::now() + chrono::Duration::minutes(minutes))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        let now = now_rfc3339();

        conn.execute(
            "UPDATE reminder
                SET snooze_until = ?3,
                    updated_at   = ?4,
                    revision     = revision + 1
              WHERE task_id = ?1 AND offset_seconds = ?2 AND deleted_at IS NULL",
            params![task_id, offset_seconds, until, now],
        )?;

        Ok(Some(minutes))
    }

    /// 清除某个档位的稍后提醒状态。
    pub fn clear_snooze(&self, task_id: &str, offset_seconds: i64) -> Result<(), RepoError> {
        let conn = self.db.lock()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE reminder
                SET snooze_until = NULL,
                    updated_at   = ?3,
                    revision     = revision + 1
              WHERE task_id = ?1 AND offset_seconds = ?2 AND deleted_at IS NULL",
            params![task_id, offset_seconds, now],
        )?;
        Ok(())
    }

    // ===================== 错过的提醒 =====================

    /// 记录一条错过的提醒。
    ///
    /// 同一任务同一时刻只记录一次：靠 `(task_id, scheduled_at)` 的存在性判断。
    /// 否则每次启动都会重复插入，用户会看到"你错过了 3 个提醒"变成"9 个"。
    pub fn record_missed(
        &self,
        task_id: &str,
        task_title: &str,
        scheduled_at: &chrono::DateTime<chrono::FixedOffset>,
    ) -> Result<(), RepoError> {
        let conn = self.db.lock()?;

        let existing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM missed_reminder
              WHERE task_id = ?1 AND scheduled_at = ?2",
            params![task_id, scheduled_at.to_rfc3339()],
            |r| r.get(0),
        )?;
        if existing > 0 {
            return Ok(());
        }

        let now = now_rfc3339();
        conn.execute(
            "INSERT INTO missed_reminder (
                id, task_id, task_title, scheduled_at, detected_at, is_acknowledged,
                created_at, updated_at, deleted_at, revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, 0, ?5, ?5, NULL, 1)",
            params![
                new_id(),
                task_id,
                task_title,
                scheduled_at.to_rfc3339(),
                now
            ],
        )?;
        Ok(())
    }

    /// 未确认的错过记录，按本应触发的时间升序。
    pub fn pending_missed(&self) -> Result<Vec<MissedReminderRow>, RepoError> {
        let conn = self.db.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, task_title, scheduled_at, detected_at, is_acknowledged
               FROM missed_reminder
              WHERE deleted_at IS NULL AND is_acknowledged = 0
              ORDER BY scheduled_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(MissedReminderRow {
                id: r.get(0)?,
                task_id: r.get(1)?,
                task_title: r.get(2)?,
                scheduled_at: r.get(3)?,
                detected_at: r.get(4)?,
                is_acknowledged: r.get::<_, i64>(5)? != 0,
            })
        })?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 确认一条错过记录（用户点"知道了"）。
    pub fn acknowledge_missed(&self, id: &str) -> Result<(), RepoError> {
        let conn = self.db.lock()?;
        let now = now_rfc3339();
        conn.execute(
            "UPDATE missed_reminder
                SET is_acknowledged = 1,
                    updated_at = ?2,
                    revision   = revision + 1
              WHERE id = ?1",
            params![id, now],
        )?;
        Ok(())
    }

    /// 全部确认。
    pub fn acknowledge_all_missed(&self) -> Result<usize, RepoError> {
        let conn = self.db.lock()?;
        let now = now_rfc3339();
        let n = conn.execute(
            "UPDATE missed_reminder
                SET is_acknowledged = 1,
                    updated_at = ?1,
                    revision   = revision + 1
              WHERE deleted_at IS NULL AND is_acknowledged = 0",
            params![now],
        )?;
        Ok(n)
    }

    /// 读取某个档位是否有待处理的稍后提醒。
    pub fn snooze_of(
        &self,
        task_id: &str,
        offset_seconds: i64,
    ) -> Result<Option<String>, RepoError> {
        let conn = self.db.lock()?;
        let v = conn
            .query_row(
                "SELECT snooze_until FROM reminder
                  WHERE task_id = ?1 AND offset_seconds = ?2 AND deleted_at IS NULL",
                params![task_id, offset_seconds],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|e| RepoError::Db(DbError::Sqlite(e)))?;
        Ok(v.flatten())
    }
}

/// 按任务的时间类型写入默认提醒档位。
///
/// 抽成接收 `&Transaction` 的自由函数，是为了让"首次创建"与"重置"两条路径
/// 共用同一份逻辑，同时避免在已持有连接锁的情况下再次 `lock()`
/// —— `std::sync::Mutex` 不可重入，那会直接死锁。
fn insert_tiers(
    tx: &rusqlite::Transaction<'_>,
    task: &Task,
    settings: &AppSettings,
) -> Result<(), RepoError> {
    let offsets: &[i64] = match task.time_kind {
        // 分级提醒是为截止型任务设计的：截止前 1 天 / 前 1 小时 / 到点
        TimeKind::BeforeDeadline => &settings.deadline_offsets_seconds,
        // 时间点与重复任务只在到点提醒 —— 用户说"3 点开会"就是 3 点
        TimeKind::AtTime | TimeKind::Recurring => &settings.point_offsets_seconds,
        // 全天任务同样只提醒一次，其基准时刻由调度器规整到设定钟点
        TimeKind::AllDay => &settings.point_offsets_seconds,
    };

    let now = now_rfc3339();
    for offset in offsets {
        tx.execute(
            "INSERT INTO reminder (
                id, task_id, offset_seconds, snooze_until, is_enabled,
                last_fired_at, created_at, updated_at, deleted_at, revision
             ) VALUES (?1, ?2, ?3, NULL, 1, NULL, ?4, ?4, NULL, 1)",
            params![new_id(), task.id, offset, now],
        )?;
    }
    Ok(())
}

fn map_tier(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReminderTierRow> {
    Ok(ReminderTierRow {
        id: row.get(0)?,
        task_id: row.get(1)?,
        offset_seconds: row.get(2)?,
        snooze_until: row.get(3)?,
        is_enabled: row.get::<_, i64>(4)? != 0,
        last_fired_at: row.get(5)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::NewTask;

    fn setup() -> Db {
        Db::open_in_memory().expect("建库失败")
    }

    fn make_task(db: &Db, kind: TimeKind, due: Option<&str>, deadline: Option<&str>) -> Task {
        let mut input = NewTask::inbox("测试任务");
        input.time_kind = kind;
        input.due_at = due.map(|s| s.to_string());
        input.deadline_at = deadline.map(|s| s.to_string());
        // 重复任务需要规则，这里用不到就不设
        if kind == TimeKind::Recurring {
            input.time_kind = TimeKind::AtTime;
        }
        crate::repo::task_repo::TaskRepo::new(db)
            .create(input)
            .expect("建任务失败")
    }

    #[test]
    fn deadline_task_gets_three_tiers() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::BeforeDeadline,
            None,
            Some("2026-09-30T18:00:00+08:00"),
        );

        repo.create_defaults(&task).unwrap();
        let tiers = repo.tiers_of(&task.id).unwrap();

        assert_eq!(tiers.len(), 3, "截止型任务应有 3 个档位");
        let offsets: Vec<i64> = tiers.iter().map(|t| t.offset_seconds).collect();
        assert_eq!(
            offsets,
            vec![-86400, -3600, 0],
            "应为前 1 天/前 1 小时/到点（单位：秒）"
        );
        assert!(tiers.iter().all(|t| t.is_enabled));
    }

    #[test]
    fn point_task_gets_single_tier() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );

        repo.create_defaults(&task).unwrap();
        let tiers = repo.tiers_of(&task.id).unwrap();
        assert_eq!(tiers.len(), 1);
        assert_eq!(tiers[0].offset_seconds, 0, "时间点任务只到点提醒");
    }

    /// 重复调用不应产生重复档位 —— 这让调用方可以无脑调用而不必先查。
    #[test]
    fn create_defaults_is_idempotent() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::BeforeDeadline,
            None,
            Some("2026-09-30T18:00:00+08:00"),
        );

        repo.create_defaults(&task).unwrap();
        repo.create_defaults(&task).unwrap();
        repo.create_defaults(&task).unwrap();

        assert_eq!(repo.tiers_of(&task.id).unwrap().len(), 3, "不应重复插入");
    }

    #[test]
    fn bulk_load_pairs_tasks_with_their_tiers() {
        let db = setup();
        let repo = ReminderRepo::new(&db);

        let a = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );
        let b = make_task(
            &db,
            TimeKind::BeforeDeadline,
            None,
            Some("2026-10-01T09:00:00+08:00"),
        );
        repo.create_defaults(&a).unwrap();
        repo.create_defaults(&b).unwrap();

        let pairs = repo.tasks_with_reminders().unwrap();
        assert_eq!(pairs.len(), 2);

        for (task, tiers) in &pairs {
            if task.id == a.id {
                assert_eq!(tiers.len(), 1);
            } else {
                assert_eq!(tiers.len(), 3);
            }
        }
    }

    /// 已完成的任务不应再被调度 —— 否则用户会收到"已完成的事"的提醒。
    #[test]
    fn completed_tasks_are_excluded_from_scheduling() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );
        repo.create_defaults(&task).unwrap();

        crate::repo::task_repo::TaskRepo::new(&db)
            .complete(&task.id)
            .unwrap();

        assert!(
            repo.tasks_with_reminders().unwrap().is_empty(),
            "已完成任务不应出现在调度集合中"
        );
    }

    #[test]
    fn mark_fired_records_timestamp() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );
        repo.create_defaults(&task).unwrap();

        assert!(repo.tiers_of(&task.id).unwrap()[0].last_fired_at.is_none());
        repo.mark_fired(&task.id, 0).unwrap();
        assert!(repo.tiers_of(&task.id).unwrap()[0].last_fired_at.is_some());
    }

    #[test]
    fn snooze_roundtrip() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );
        repo.create_defaults(&task).unwrap();

        assert!(repo.snooze_of(&task.id, 0).unwrap().is_none());

        repo.snooze(&task.id, 0, "2026-09-30T15:10:00+08:00")
            .unwrap();
        assert_eq!(
            repo.snooze_of(&task.id, 0).unwrap().as_deref(),
            Some("2026-09-30T15:10:00+08:00")
        );

        repo.clear_snooze(&task.id, 0).unwrap();
        assert!(repo.snooze_of(&task.id, 0).unwrap().is_none());
    }

    // ===================== 错过的提醒 =====================

    #[test]
    fn missed_records_are_deduplicated() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-30T15:00:00+08:00").unwrap();

        repo.record_missed(&task.id, &task.title, &at).unwrap();
        repo.record_missed(&task.id, &task.title, &at).unwrap();
        repo.record_missed(&task.id, &task.title, &at).unwrap();

        assert_eq!(
            repo.pending_missed().unwrap().len(),
            1,
            "同一任务同一时刻只应记录一次，否则每次启动都会重复累加"
        );
    }

    #[test]
    fn acknowledge_removes_from_pending() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );
        let at = chrono::DateTime::parse_from_rfc3339("2026-09-30T15:00:00+08:00").unwrap();
        repo.record_missed(&task.id, &task.title, &at).unwrap();

        let pending = repo.pending_missed().unwrap();
        assert_eq!(pending.len(), 1);
        assert!(!pending[0].is_acknowledged);

        repo.acknowledge_missed(&pending[0].id).unwrap();
        assert!(repo.pending_missed().unwrap().is_empty());
    }

    #[test]
    fn acknowledge_all_clears_everything() {
        let db = setup();
        let repo = ReminderRepo::new(&db);
        let task = make_task(
            &db,
            TimeKind::AtTime,
            Some("2026-09-30T15:00:00+08:00"),
            None,
        );

        for hour in [15, 16, 17] {
            let at =
                chrono::DateTime::parse_from_rfc3339(&format!("2026-09-30T{hour}:00:00+08:00"))
                    .unwrap();
            repo.record_missed(&task.id, &task.title, &at).unwrap();
        }
        assert_eq!(repo.pending_missed().unwrap().len(), 3);

        let n = repo.acknowledge_all_missed().unwrap();
        assert_eq!(n, 3);
        assert!(repo.pending_missed().unwrap().is_empty());
    }
}
