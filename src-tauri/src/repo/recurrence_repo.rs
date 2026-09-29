//! 重复规则的读写。
//!
//! 与 `task_repo` 一样，所有对本表的写操作都必须经过这里，
//! 以保证 `revision` 与 `changelog` 等同步所需的副作用不会被漏掉。

use rusqlite::{params, OptionalExtension, Row};

use crate::db::connection::{Db, DbError};
use crate::domain::recurrence::{Freq, RecurrenceRule};
use crate::domain::time::{new_id, now_rfc3339};
use crate::repo::task_repo::RepoError;

pub struct RecurrenceRepo<'a> {
    db: &'a Db,
}

impl<'a> RecurrenceRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 保存一条新的重复规则，返回其 ID。
    pub fn create(&self, rule: &RecurrenceRule) -> Result<String, RepoError> {
        validate(rule)?;

        let conn = self.db.lock()?;
        let id = if rule.id.is_empty() {
            new_id()
        } else {
            rule.id.clone()
        };
        let now = now_rfc3339();

        conn.execute(
            "INSERT INTO recurrence_rule (
                id, freq, interval, by_weekdays, by_monthday,
                until_date, max_count, at_time_of_day, tz,
                created_at, updated_at, deleted_at, revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, NULL, 1)",
            params![
                id,
                rule.freq.as_db(),
                rule.interval,
                rule.by_weekdays,
                rule.by_monthday,
                rule.until_date,
                rule.max_count,
                rule.at_time_of_day,
                rule.tz,
                now,
                now,
            ],
        )?;

        Ok(id)
    }

    /// 按 ID 读取规则。
    pub fn get(&self, id: &str) -> Result<Option<RecurrenceRule>, RepoError> {
        let conn = self.db.lock()?;
        let rule = conn
            .query_row(
                &format!("{SELECT_RULE} WHERE id = ?1 AND deleted_at IS NULL"),
                params![id],
                map_rule,
            )
            .optional()
            .map_err(|e| RepoError::Db(DbError::Sqlite(e)))?;
        Ok(rule)
    }
}

const SELECT_RULE: &str = "SELECT
    id, freq, interval, by_weekdays, by_monthday,
    until_date, max_count, at_time_of_day, tz,
    created_at, updated_at, deleted_at, revision
  FROM recurrence_rule";

fn map_rule(row: &Row<'_>) -> rusqlite::Result<RecurrenceRule> {
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

/// 入库前校验。
///
/// 与 SQL 的 CHECK 约束重叠是有意的：这里能给出中文提示，
/// 而 CHECK 只能抛出 `constraint failed`。
fn validate(rule: &RecurrenceRule) -> Result<(), RepoError> {
    if rule.interval < 1 {
        return Err(RepoError::Invalid("重复间隔必须至少为 1".into()));
    }

    if rule.freq == Freq::Weekly || rule.freq == Freq::EveryNWeeks {
        match rule.by_weekdays {
            Some(0) | None => {
                return Err(RepoError::Invalid("每周重复必须指定至少一个星期几".into()));
            }
            Some(_) => {}
        }
    }

    if rule.freq == Freq::Monthly {
        match rule.by_monthday {
            Some(d) if (1..=31).contains(&d) => {}
            Some(d) => {
                return Err(RepoError::Invalid(format!(
                    "每月重复的日期必须在 1–31 之间，当前为 {d}"
                )));
            }
            None => return Err(RepoError::Invalid("每月重复必须指定日期".into())),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::recurrence::encode_weekdays;
    use chrono::Weekday;

    fn fixture() -> Db {
        Db::open_in_memory().expect("建库失败")
    }

    fn base(freq: Freq) -> RecurrenceRule {
        RecurrenceRule {
            id: String::new(), // 交给仓储生成
            freq,
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
        }
    }

    #[test]
    fn create_and_get_roundtrip() {
        let db = fixture();
        let repo = RecurrenceRepo::new(&db);

        let mut rule = base(Freq::Weekly);
        rule.by_weekdays = Some(encode_weekdays(&[Weekday::Mon, Weekday::Fri]));
        rule.max_count = Some(10);

        let id = repo.create(&rule).unwrap();
        let loaded = repo.get(&id).unwrap().expect("应当能读到");

        assert_eq!(loaded.freq, Freq::Weekly);
        assert_eq!(loaded.by_weekdays, rule.by_weekdays);
        assert_eq!(loaded.max_count, Some(10));
        assert_eq!(loaded.at_time_of_day.as_deref(), Some("09:00"));
    }

    /// 每周规则缺星期几必须被拒绝，否则引擎会得到空掩码而永不触发。
    #[test]
    fn weekly_without_weekdays_is_rejected() {
        let db = fixture();
        let repo = RecurrenceRepo::new(&db);

        let rule = base(Freq::Weekly);
        assert!(matches!(
            repo.create(&rule).expect_err("应当被拒绝"),
            RepoError::Invalid(_)
        ));
    }

    #[test]
    fn weekly_with_empty_mask_is_rejected() {
        let db = fixture();
        let repo = RecurrenceRepo::new(&db);

        let mut rule = base(Freq::Weekly);
        rule.by_weekdays = Some(0);
        assert!(matches!(
            repo.create(&rule).expect_err("空掩码应被拒绝"),
            RepoError::Invalid(_)
        ));
    }

    #[test]
    fn monthly_requires_valid_monthday() {
        let db = fixture();
        let repo = RecurrenceRepo::new(&db);

        // 缺日期
        assert!(repo.create(&base(Freq::Monthly)).is_err());

        // 超出范围
        let mut bad = base(Freq::Monthly);
        bad.by_monthday = Some(32);
        assert!(repo.create(&bad).is_err());

        // 合法
        let mut ok = base(Freq::Monthly);
        ok.by_monthday = Some(15);
        assert!(repo.create(&ok).is_ok());
    }

    #[test]
    fn interval_below_one_is_rejected() {
        let db = fixture();
        let repo = RecurrenceRepo::new(&db);

        let mut rule = base(Freq::EveryNDays);
        rule.interval = 0;
        assert!(matches!(
            repo.create(&rule).expect_err("应当被拒绝"),
            RepoError::Invalid(_)
        ));
    }

    /// 所有频率都要能正确往返，避免某个频率的字符串映射写错。
    #[test]
    fn all_freqs_roundtrip_through_db() {
        let db = fixture();
        let repo = RecurrenceRepo::new(&db);

        for freq in [
            Freq::Daily,
            Freq::Weekdays,
            Freq::Weekly,
            Freq::EveryNDays,
            Freq::EveryNWeeks,
            Freq::Monthly,
        ] {
            let mut rule = base(freq);
            // 补齐各频率的必填参数
            if matches!(freq, Freq::Weekly | Freq::EveryNWeeks) {
                rule.by_weekdays = Some(encode_weekdays(&[Weekday::Tue]));
            }
            if freq == Freq::Monthly {
                rule.by_monthday = Some(1);
            }

            let id = repo.create(&rule).expect("创建失败");
            let loaded = repo.get(&id).unwrap().expect("读取失败");
            assert_eq!(loaded.freq, freq, "{freq:?} 往返后不一致");
        }
    }
}
