//! 应用设置的读取与写入。
//!
//! 设置以键值对存在 `setting` 表里，而非为每项建一列。理由：设置项会随功能
//! 演进而增减，用键值对意味着新增一项设置不需要 schema 迁移 —— 而对一个
//! 已经发布、用户已有数据的应用来说，每次迁移都是一次风险。
//!
//! 代价是失去了类型约束与默认值约束，因此这里用 `AppSettings` 结构体统一封装：
//! 所有读取都经过它，缺键、值非法、类型不符都在这里降级为默认值，调用方拿到的
//! 永远是完整可用的配置。

use serde::{Deserialize, Serialize};

use crate::db::connection::{Db, DbError};
use crate::domain::time::now_rfc3339;
use crate::repo::task_repo::RepoError;

/// 全部应用设置。
///
/// 每个字段都必须有合理的默认值：用户从未打开过设置页时，应用也应完全可用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// 通知总开关。
    pub notifications_enabled: bool,

    /// 勿扰时段的起始小时（0–23）。该区间内不弹通知。
    pub quiet_hours_start: u32,
    /// 勿扰时段的结束小时（0–23）。
    pub quiet_hours_end: u32,
    /// 勿扰开关。
    pub quiet_hours_enabled: bool,

    /// 截止型任务的默认提醒档位（相对截止时间的分钟偏移，负值为提前）。
    pub deadline_offsets_seconds: Vec<i64>,
    /// 时间点 / 重复型任务的默认提醒档位。
    pub point_offsets_seconds: Vec<i64>,

    /// 全天任务的提醒钟点。
    pub all_day_hour: u32,
    pub all_day_minute: u32,

    /// 关闭窗口时最小化到托盘而不是退出。
    pub close_to_tray: bool,

    /// 主题：system / light / dark。
    pub theme: String,

    /// 稍后提醒的默认时长（分钟）。
    pub snooze_minutes: i64,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            notifications_enabled: true,

            quiet_hours_enabled: false,
            quiet_hours_start: 22,
            quiet_hours_end: 8,

            // 提示词指定的默认分级策略：截止前 1 天 / 前 1 小时 / 到点
            deadline_offsets_seconds: vec![-86400, -3600, 0],
            // 时间点任务只到点提醒一次 —— 用户说"3 点开会"就是 3 点，
            // 提前响反而会让人以为时间到了。
            point_offsets_seconds: vec![0],

            all_day_hour: 9,
            all_day_minute: 0,

            close_to_tray: true,
            theme: "system".into(),
            snooze_minutes: 10,
        }
    }
}

impl AppSettings {
    /// 判断某个时刻是否处于勿扰时段。
    ///
    /// 必须处理**跨午夜**的区间（如 22:00–08:00）：这是最常见也最容易写错的配置，
    /// 用简单的 `start <= h && h < end` 判断会让它在跨午夜时完全失效。
    pub fn in_quiet_hours(&self, hour: u32) -> bool {
        if !self.quiet_hours_enabled {
            return false;
        }
        let s = self.quiet_hours_start % 24;
        let e = self.quiet_hours_end % 24;

        if s == e {
            // 起止相同视为"全天静默"。这是一个明确的语义选择，
            // 而不是"不静默" —— 用户把两端设成同一时刻显然是想要静音。
            return true;
        }
        if s < e {
            s <= hour && hour < e
        } else {
            // 跨午夜：22 点至次日 8 点
            hour >= s || hour < e
        }
    }
}

/// 设置的读写入口。
pub struct SettingsRepo<'a> {
    db: &'a Db,
}

impl<'a> SettingsRepo<'a> {
    pub fn new(db: &'a Db) -> Self {
        Self { db }
    }

    /// 读取全部设置。缺失或非法的项一律回落到默认值。
    pub fn load(&self) -> Result<AppSettings, RepoError> {
        let conn = self.db.lock()?;
        let mut stmt = conn.prepare("SELECT key, value FROM setting")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;

        let mut map = std::collections::HashMap::new();
        for row in rows {
            let (k, v) = row?;
            map.insert(k, v);
        }
        drop(stmt);
        drop(conn);

        Ok(Self::from_map(&map))
    }

    /// 从键值映射构造设置，逐项容错。
    ///
    /// 单独抽出来是为了可测试：不需要数据库就能验证"某个值畸形时会怎样"。
    fn from_map(map: &std::collections::HashMap<String, String>) -> AppSettings {
        let d = AppSettings::default();

        // 小工具：解析失败就用默认值。设置项读不出来绝不该让应用启动失败。
        let bool_of = |k: &str, fallback: bool| {
            map.get(k)
                .and_then(|v| match v.as_str() {
                    "true" | "1" => Some(true),
                    "false" | "0" => Some(false),
                    _ => None,
                })
                .unwrap_or(fallback)
        };
        let u32_of = |k: &str, fallback: u32| {
            map.get(k)
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| *v < 24)
                .unwrap_or(fallback)
        };
        let i64_of = |k: &str, fallback: i64| {
            map.get(k)
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(fallback)
        };
        let offsets_of = |k: &str, fallback: &[i64]| {
            map.get(k)
                .and_then(|v| serde_json::from_str::<Vec<i64>>(v).ok())
                // 空档位等于"永不提醒"，几乎肯定是配置损坏而非用户意图，
                // 因此回落到默认值而不是接受空数组。
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| fallback.to_vec())
        };

        AppSettings {
            notifications_enabled: bool_of("notifications_enabled", d.notifications_enabled),

            quiet_hours_enabled: bool_of("quiet_hours_enabled", d.quiet_hours_enabled),
            quiet_hours_start: u32_of("quiet_hours_start", d.quiet_hours_start),
            quiet_hours_end: u32_of("quiet_hours_end", d.quiet_hours_end),

            deadline_offsets_seconds: offsets_of(
                "deadline_offsets_seconds",
                &d.deadline_offsets_seconds,
            ),
            point_offsets_seconds: offsets_of("point_offsets_seconds", &d.point_offsets_seconds),

            all_day_hour: u32_of("all_day_hour", d.all_day_hour),
            all_day_minute: map
                .get("all_day_minute")
                .and_then(|v| v.parse::<u32>().ok())
                .filter(|v| *v < 60)
                .unwrap_or(d.all_day_minute),

            close_to_tray: bool_of("close_to_tray", d.close_to_tray),
            theme: map
                .get("theme")
                .filter(|v| matches!(v.as_str(), "system" | "light" | "dark"))
                .cloned()
                .unwrap_or(d.theme),
            snooze_minutes: i64_of("snooze_minutes", d.snooze_minutes).clamp(1, 24 * 60),
        }
    }

    /// 写入全部设置（覆盖式）。
    pub fn save(&self, s: &AppSettings) -> Result<(), RepoError> {
        let mut conn = self.db.lock()?;
        let tx = conn.transaction()?;
        let now = now_rfc3339();

        let pairs: Vec<(&str, String)> = vec![
            ("notifications_enabled", s.notifications_enabled.to_string()),
            ("quiet_hours_enabled", s.quiet_hours_enabled.to_string()),
            ("quiet_hours_start", s.quiet_hours_start.to_string()),
            ("quiet_hours_end", s.quiet_hours_end.to_string()),
            (
                "deadline_offsets_seconds",
                serde_json::to_string(&s.deadline_offsets_seconds)
                    .map_err(|e| RepoError::Serialize(e.to_string()))?,
            ),
            (
                "point_offsets_seconds",
                serde_json::to_string(&s.point_offsets_seconds)
                    .map_err(|e| RepoError::Serialize(e.to_string()))?,
            ),
            ("all_day_hour", s.all_day_hour.to_string()),
            ("all_day_minute", s.all_day_minute.to_string()),
            ("close_to_tray", s.close_to_tray.to_string()),
            ("theme", s.theme.clone()),
            ("snooze_minutes", s.snooze_minutes.to_string()),
        ];

        for (k, v) in pairs {
            // UPSERT：首次写入是插入，之后是更新。用一条语句表达两种情形，
            // 避免在事务里先查后写。
            tx.execute(
                "INSERT INTO setting (key, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
                rusqlite::params![k, v, now],
            )?;
        }

        tx.commit()?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum SettingsError {
    Db(DbError),
}

impl std::fmt::Display for SettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SettingsError::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SettingsError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_match_spec() {
        let d = AppSettings::default();
        assert!(d.notifications_enabled);
        assert_eq!(d.deadline_offsets_seconds, vec![-86400, -3600, 0]);
        assert_eq!(d.point_offsets_seconds, vec![0]);
        assert!(d.close_to_tray);
        assert_eq!(d.theme, "system");
    }

    #[test]
    fn empty_map_yields_defaults() {
        let s = SettingsRepo::from_map(&map(&[]));
        assert_eq!(s, AppSettings::default());
    }

    /// 值畸形时必须回落到默认值，而不是让应用启动失败。
    #[test]
    fn malformed_values_fall_back_to_defaults() {
        let s = SettingsRepo::from_map(&map(&[
            ("notifications_enabled", "maybe"),
            ("quiet_hours_start", "abc"),
            ("quiet_hours_end", "99"),
            ("deadline_offsets_seconds", "not json"),
            ("all_day_minute", "70"),
            ("theme", "neon"),
            ("snooze_minutes", "99999"),
        ]));

        let d = AppSettings::default();
        assert_eq!(s.notifications_enabled, d.notifications_enabled);
        assert_eq!(s.quiet_hours_start, d.quiet_hours_start);
        assert_eq!(s.quiet_hours_end, d.quiet_hours_end);
        assert_eq!(s.deadline_offsets_seconds, d.deadline_offsets_seconds);
        assert_eq!(s.all_day_minute, d.all_day_minute);
        assert_eq!(s.theme, d.theme);
        // 时长被 clamp 到上限而不是回落默认值
        assert_eq!(s.snooze_minutes, 24 * 60);
    }

    /// 空的提醒档位等于"永不提醒"，几乎肯定是损坏，应回落默认值。
    #[test]
    fn empty_offsets_fall_back() {
        let s = SettingsRepo::from_map(&map(&[("deadline_offsets_seconds", "[]")]));
        assert_eq!(
            s.deadline_offsets_seconds,
            AppSettings::default().deadline_offsets_seconds
        );
    }

    #[test]
    fn valid_values_are_loaded() {
        let s = SettingsRepo::from_map(&map(&[
            ("notifications_enabled", "false"),
            ("deadline_offsets_seconds", "[-1800,0]"),
            ("theme", "dark"),
            ("snooze_minutes", "5"),
        ]));
        assert!(!s.notifications_enabled);
        assert_eq!(s.deadline_offsets_seconds, vec![-1800, 0]);
        assert_eq!(s.theme, "dark");
        assert_eq!(s.snooze_minutes, 5);
    }

    // ===================== 勿扰时段 =====================

    #[test]
    fn quiet_hours_disabled_never_matches() {
        // 用结构体更新语法而非先 default() 再逐字段赋值：
        // 后者在字段多时容易漏改，也让"这个测试只关心哪个字段"变得不明显。
        let s = AppSettings {
            quiet_hours_enabled: false,
            ..Default::default()
        };
        for h in 0..24 {
            assert!(!s.in_quiet_hours(h), "{h} 点不应静默");
        }
    }

    /// 跨午夜的区间是最容易写错的配置（22:00–08:00）。
    #[test]
    fn quiet_hours_across_midnight() {
        let s = AppSettings {
            quiet_hours_enabled: true,
            quiet_hours_start: 22,
            quiet_hours_end: 8,
            ..Default::default()
        };

        for h in [22, 23, 0, 1, 7] {
            assert!(s.in_quiet_hours(h), "{h} 点应静默");
        }
        for h in [8, 12, 18, 21] {
            assert!(!s.in_quiet_hours(h), "{h} 点不应静默");
        }
    }

    #[test]
    fn quiet_hours_within_same_day() {
        let s = AppSettings {
            quiet_hours_enabled: true,
            quiet_hours_start: 13,
            quiet_hours_end: 15,
            ..Default::default()
        };

        assert!(!s.in_quiet_hours(12));
        assert!(s.in_quiet_hours(13));
        assert!(s.in_quiet_hours(14));
        assert!(
            !s.in_quiet_hours(15),
            "结束时刻本身不应静默（区间左闭右开）"
        );
    }

    /// 起止相同视为全天静默，这是明确的语义选择。
    #[test]
    fn quiet_hours_same_start_and_end_means_all_day() {
        let s = AppSettings {
            quiet_hours_enabled: true,
            quiet_hours_start: 9,
            quiet_hours_end: 9,
            ..Default::default()
        };
        for h in 0..24 {
            assert!(s.in_quiet_hours(h));
        }
    }

    // ===================== 数据库往返 =====================

    #[test]
    fn settings_roundtrip_through_db() {
        let db = Db::open_in_memory().unwrap();
        let repo = SettingsRepo::new(&db);

        // 未写入时应得到默认值
        assert_eq!(repo.load().unwrap(), AppSettings::default());

        let custom = AppSettings {
            notifications_enabled: false,
            quiet_hours_enabled: true,
            quiet_hours_start: 23,
            quiet_hours_end: 7,
            deadline_offsets_seconds: vec![-7200, -600, 0],
            point_offsets_seconds: vec![-300, 0],
            all_day_hour: 8,
            all_day_minute: 30,
            close_to_tray: false,
            theme: "dark".into(),
            snooze_minutes: 30,
        };
        repo.save(&custom).unwrap();

        assert_eq!(repo.load().unwrap(), custom);

        // 再保存一次应更新而非报主键冲突
        let mut again = custom.clone();
        again.theme = "light".into();
        repo.save(&again).unwrap();
        assert_eq!(repo.load().unwrap().theme, "light");
    }
}
