//! 完成情况的统计聚合。
//!
//! 聚合放在 SQL 里做而不是读出来在 Rust 里算：完成记录可能累积到几千条，
//! 全部读进内存再统计会白白占用内存，而本项目的核心指标之一就是内存占用。

use serde::{Deserialize, Serialize};

use crate::db::connection::Db;
use crate::domain::time::now_rfc3339;
use crate::repo::task_repo::RepoError;

/// 某一天的完成数量。用于统计页的柱状图。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DailyCount {
    /// `YYYY-MM-DD`
    pub date: String,
    pub count: i64,
}

/// 某个任务的完成次数与最近一次完成时间。
///
/// 回答用户"这个月我完成了多少次锻炼"这类问题。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskStreak {
    pub task_id: String,
    pub task_title: String,
    pub total: i64,
    /// 该任务的完成记录里最早与最晚的时刻，用于展示坚持的时间跨度。
    pub first_at: Option<String>,
    pub last_at: Option<String>,
}

/// 总体统计概览。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatsOverview {
    /// 未完成任务数
    pub unfinished: i64,
    /// 已完成任务数（任务维度，不含重复任务的每一轮）
    pub completed: i64,
    /// 完成记录总条数（含重复任务的每一轮）
    pub total_completions: i64,
    /// 今天完成了多少
    pub today_completions: i64,
    /// 最近 7 天完成了多少
    pub last_7_days_completions: i64,
    /// 最近 30 天完成了多少
    pub last_30_days_completions: i64,
    /// 逾期未完成的数量
    pub overdue: i64,
}

pub struct StatsRepo<'a> {
    db: &'a Db,
}

impl<'a> StatsRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 总体概览。
    pub fn overview(&self) -> Result<StatsOverview, RepoError> {
        let conn = self.db.lock()?;
        let now = now_rfc3339();

        let unfinished: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task WHERE deleted_at IS NULL AND is_completed = 0",
            [],
            |r| r.get(0),
        )?;

        let completed: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task WHERE deleted_at IS NULL AND is_completed = 1",
            [],
            |r| r.get(0),
        )?;

        let total_completions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task_completion WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )?;

        // 用 SQLite 的 date() 比较日期而不是在 Rust 里算区间：
        // 完成记录存的是带偏移的 RFC3339 文本，`date()` 能正确取出其日期部分。
        // 而 'now' 是 SQLite 的本地时间，与写入时的本地时区一致。
        let count_since = |days: &str| -> rusqlite::Result<i64> {
            conn.query_row(
                "SELECT COUNT(*) FROM task_completion
                  WHERE deleted_at IS NULL
                    AND date(completed_at) >= date('now', ?1)",
                rusqlite::params![days],
                |r| r.get(0),
            )
        };

        let today_completions = count_since("+0 days")?;
        let last_7_days_completions = count_since("-6 days")?;
        let last_30_days_completions = count_since("-29 days")?;

        // 逾期 = 未完成、且时间基准已经过去。
        // 时间基准按类型取：截止型看 deadline_at，其余看 due_at。
        let overdue: i64 = conn.query_row(
            "SELECT COUNT(*) FROM task
              WHERE deleted_at IS NULL
                AND is_completed = 0
                AND (
                  (time_kind = 'before_deadline' AND deadline_at IS NOT NULL AND deadline_at < ?1)
                  OR
                  (time_kind <> 'before_deadline' AND due_at IS NOT NULL AND due_at < ?1)
                )",
            rusqlite::params![now],
            |r| r.get(0),
        )?;

        Ok(StatsOverview {
            unfinished,
            completed,
            total_completions,
            today_completions,
            last_7_days_completions,
            last_30_days_completions,
            overdue,
        })
    }

    /// 最近 `days` 天每天的完成数量，按日期升序。
    ///
    /// **包含没有完成记录的日期**（数量为 0）。这一点很重要：若只返回有记录的
    /// 日子，柱状图会把"空了三天"显示成连续的三根柱子，用户看到的是误导性的图。
    /// 补零在 Rust 侧做，因为 SQLite 生成连续日期序列需要递归 CTE，可读性差。
    pub fn daily_counts(&self, days: i64) -> Result<Vec<DailyCount>, RepoError> {
        let days = days.clamp(1, 365);
        let conn = self.db.lock()?;

        let mut stmt = conn.prepare(
            "SELECT date(completed_at) AS d, COUNT(*) AS n
               FROM task_completion
              WHERE deleted_at IS NULL
                AND date(completed_at) >= date('now', ?1)
              GROUP BY d",
        )?;

        let offset = format!("-{} days", days - 1);
        let rows = stmt.query_map(rusqlite::params![offset], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;

        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (d, n) = row?;
            map.insert(d, n);
        }

        // 补零：从 days-1 天前到今天逐日生成
        let today = chrono::Local::now().date_naive();
        let mut out = Vec::with_capacity(days as usize);
        for i in (0..days).rev() {
            let date = today - chrono::Duration::days(i);
            let key = date.format("%Y-%m-%d").to_string();
            let count = map.get(&key).copied().unwrap_or(0);
            out.push(DailyCount { date: key, count });
        }

        Ok(out)
    }

    /// 按任务汇总完成次数，降序排列。
    ///
    /// 只统计**仍有完成记录**的任务；已软删除的任务不计入，
    /// 但它的完成记录本身也不会被删（软删除只标记任务），
    /// 因此这里用 INNER JOIN 把已删任务排除掉。
    pub fn by_task(&self, limit: i64) -> Result<Vec<TaskStreak>, RepoError> {
        let limit = limit.clamp(1, 200);
        let conn = self.db.lock()?;

        let mut stmt = conn.prepare(
            "SELECT t.id, t.title, COUNT(c.id) AS total,
                    MIN(c.completed_at) AS first_at,
                    MAX(c.completed_at) AS last_at
               FROM task t
               JOIN task_completion c ON c.task_id = t.id AND c.deleted_at IS NULL
              WHERE t.deleted_at IS NULL
              GROUP BY t.id, t.title
              ORDER BY total DESC, t.title ASC
              LIMIT ?1",
        )?;

        let rows = stmt.query_map(rusqlite::params![limit], |r| {
            Ok(TaskStreak {
                task_id: r.get(0)?,
                task_title: r.get(1)?,
                total: r.get(2)?,
                first_at: r.get(3)?,
                last_at: r.get(4)?,
            })
        })?;

        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::{NewTask, TimeKind};
    use crate::repo::task_repo::TaskRepo;

    fn db() -> Db {
        Db::open_in_memory().expect("建库失败")
    }

    fn make(db: &Db, title: &str) -> crate::domain::task::Task {
        TaskRepo::new(db)
            .create(NewTask::inbox(title))
            .expect("建任务失败")
    }

    #[test]
    fn empty_database_yields_zeroes() {
        let d = db();
        let o = StatsRepo::new(&d).overview().unwrap();
        assert_eq!(o.unfinished, 0);
        assert_eq!(o.completed, 0);
        assert_eq!(o.total_completions, 0);
        assert_eq!(o.today_completions, 0);
        assert_eq!(o.overdue, 0);
    }

    #[test]
    fn counts_unfinished_and_completed_separately() {
        let d = db();
        let repo = TaskRepo::new(&d);
        let a = make(&d, "甲");
        make(&d, "乙");
        make(&d, "丙");

        repo.complete(&a.id).unwrap();

        let o = StatsRepo::new(&d).overview().unwrap();
        assert_eq!(o.unfinished, 2);
        assert_eq!(o.completed, 1);
        assert_eq!(o.total_completions, 1);
    }

    /// 今天完成的应计入"今天"。
    #[test]
    fn today_completions_are_counted() {
        let d = db();
        let a = make(&d, "今天做的");
        TaskRepo::new(&d).complete(&a.id).unwrap();

        let o = StatsRepo::new(&d).overview().unwrap();
        assert_eq!(o.today_completions, 1);
        assert_eq!(o.last_7_days_completions, 1);
        assert_eq!(o.last_30_days_completions, 1);
    }

    /// 撤销完成后，统计不应再计入 —— 否则"完成了 3 次"会虚高。
    #[test]
    fn uncomplete_removes_from_stats() {
        let d = db();
        let a = make(&d, "误勾的");
        let repo = TaskRepo::new(&d);
        repo.complete(&a.id).unwrap();
        repo.uncomplete(&a.id).unwrap();

        let o = StatsRepo::new(&d).overview().unwrap();
        assert_eq!(o.total_completions, 0);
        assert_eq!(o.today_completions, 0);
        assert_eq!(o.completed, 0);
    }

    /// 逾期统计必须按时间类型选对基准字段。
    #[test]
    fn overdue_uses_correct_anchor_per_kind() {
        let d = db();
        let repo = TaskRepo::new(&d);

        // 已过期的截止型任务
        let mut t1 = NewTask::inbox("过期的截止任务");
        t1.time_kind = TimeKind::BeforeDeadline;
        t1.deadline_at = Some("2020-01-01T00:00:00+08:00".into());
        repo.create(t1).unwrap();

        // 已过期的时间点任务
        let mut t2 = NewTask::inbox("过期的时间点任务");
        t2.time_kind = TimeKind::AtTime;
        t2.due_at = Some("2020-01-01T00:00:00+08:00".into());
        repo.create(t2).unwrap();

        // 未来的任务，不算逾期
        let mut t3 = NewTask::inbox("未来的任务");
        t3.time_kind = TimeKind::AtTime;
        t3.due_at = Some("2099-01-01T00:00:00+08:00".into());
        repo.create(t3).unwrap();

        // 没有时间的任务，不算逾期
        make(&d, "收件箱任务");

        let o = StatsRepo::new(&d).overview().unwrap();
        assert_eq!(o.overdue, 2, "两条已过期的应计入，未来与无时间的都不算");
    }

    /// 已完成的任务即使过期也不应算作逾期。
    #[test]
    fn completed_task_is_not_overdue() {
        let d = db();
        let repo = TaskRepo::new(&d);

        let mut t = NewTask::inbox("过期但已完成");
        t.time_kind = TimeKind::AtTime;
        t.due_at = Some("2020-01-01T00:00:00+08:00".into());
        let task = repo.create(t).unwrap();
        repo.complete(&task.id).unwrap();

        assert_eq!(StatsRepo::new(&d).overview().unwrap().overdue, 0);
    }

    /// daily_counts 必须**包含没有记录的日期**（补零），
    /// 否则柱状图会把"空了三天"画成连续三根柱子，误导用户。
    #[test]
    fn daily_counts_includes_zero_days() {
        let d = db();
        let a = make(&d, "今天做的");
        TaskRepo::new(&d).complete(&a.id).unwrap();

        let counts = StatsRepo::new(&d).daily_counts(7).unwrap();
        assert_eq!(counts.len(), 7, "应返回完整的 7 天，含没有记录的日子");

        // 最后一个是今天
        assert_eq!(counts.last().unwrap().count, 1);
        // 前面 6 天都应是 0
        for c in &counts[..6] {
            assert_eq!(c.count, 0, "{} 应为 0", c.date);
        }
    }

    /// 日期必须升序，柱状图才按时间从左到右排列。
    #[test]
    fn daily_counts_are_ascending() {
        let d = db();
        let counts = StatsRepo::new(&d).daily_counts(30).unwrap();
        assert_eq!(counts.len(), 30);
        for pair in counts.windows(2) {
            assert!(pair[0].date < pair[1].date, "日期应严格升序");
        }
    }

    /// 按任务汇总并降序排列。
    ///
    /// 用**重复任务**来产生多条完成记录：普通任务第二次 `complete` 是幂等的
    /// （它已经完成，不会再记录一次），只有重复任务每勾一次才会追加一条流水。
    /// 早期版本误用"完成再撤销"的循环来造数据，结果撤销把记录删掉了，
    /// 统计自然就排错了序。
    #[test]
    fn by_task_aggregates_and_sorts() {
        let d = db();
        let repo = TaskRepo::new(&d);
        let rec_repo = crate::repo::recurrence_repo::RecurrenceRepo::new(&d);

        // 两个每日重复任务
        let rule = crate::domain::recurrence::RecurrenceRule {
            id: String::new(),
            freq: crate::domain::recurrence::Freq::Daily,
            interval: 1,
            by_weekdays: None,
            by_monthday: None,
            until_date: None,
            max_count: None,
            at_time_of_day: Some("09:00".into()),
            tz: None,
            created_at: String::new(),
            updated_at: String::new(),
            deleted_at: None,
            revision: 1,
        };
        let rule_id = rec_repo.create(&rule).unwrap();

        let make_recurring = |title: &str| {
            let mut input = NewTask::inbox(title);
            input.time_kind = TimeKind::Recurring;
            input.due_at = Some("2026-09-30T09:00:00+08:00".into());
            input.recurrence_id = Some(rule_id.clone());
            repo.create(input).unwrap()
        };

        let exercise = make_recurring("锻炼");
        let reading = make_recurring("读书");

        // 锻炼完成 3 轮，读书完成 1 轮
        for _ in 0..3 {
            repo.complete(&exercise.id).unwrap();
        }
        repo.complete(&reading.id).unwrap();

        let streaks = StatsRepo::new(&d).by_task(10).unwrap();
        assert_eq!(streaks.len(), 2);
        assert_eq!(streaks[0].task_title, "锻炼", "完成次数多的应排在前面");
        assert_eq!(streaks[0].total, 3);
        assert_eq!(streaks[1].task_title, "读书");
        assert_eq!(streaks[1].total, 1);
        assert!(streaks[0].first_at.is_some());
        assert!(streaks[0].last_at.is_some());
    }

    /// 重复任务每勾一次都应计入统计 —— 这是"这个月完成了多少次"的数据来源。
    #[test]
    fn recurring_completions_accumulate_in_stats() {
        let d = db();
        let repo = TaskRepo::new(&d);
        let rec_repo = crate::repo::recurrence_repo::RecurrenceRepo::new(&d);

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
        input.due_at = Some("2026-09-30T08:00:00+08:00".into());
        input.recurrence_id = Some(rule_id);
        let task = repo.create(input).unwrap();

        for _ in 0..5 {
            repo.complete(&task.id).unwrap();
        }

        let o = StatsRepo::new(&d).overview().unwrap();
        assert_eq!(o.total_completions, 5, "5 轮完成都应计入");
        assert_eq!(o.today_completions, 5);
        assert_eq!(o.unfinished, 1, "重复任务本身始终是未完成状态");
    }

    /// 已软删除的任务不应出现在统计里。
    #[test]
    fn deleted_task_excluded_from_by_task() {
        let d = db();
        let repo = TaskRepo::new(&d);
        let a = make(&d, "将被删除的");
        repo.complete(&a.id).unwrap();
        repo.soft_delete(&a.id).unwrap();

        assert!(
            StatsRepo::new(&d).by_task(10).unwrap().is_empty(),
            "已删除的任务不应出现在统计中"
        );
    }
}
