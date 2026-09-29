//! 提醒档位的计算。
//!
//! 与重复规则一样，本模块是纯逻辑：给定"任务的基准时间"和"提醒档位"，
//! 算出所有触发时刻。不读数据库、不看系统状态，因此可直接单元测试。
//!
//! # 为什么只存偏移而不存绝对时刻
//!
//! 数据库的 `reminder.offset_seconds` 存的是相对基准的偏移（如 -86400 表示
//! 提前一天）。这样用户改动任务时间时，所有提醒会**自动跟着平移**，
//! 不需要同步更新多条提醒记录，也不可能出现"任务改了但提醒还指着旧时间"。
//!
//! 单位是**秒**而非分钟：用户会用「10 秒后提醒我」来快速验证提醒是否工作，
//! 若单位是分钟，10 秒会被截断成 0，表现为"输入了时间却没有任何提醒"。

use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime};
use serde::{Deserialize, Serialize};

use crate::domain::task::{AnchorField, TimeKind};

/// 一条提醒档位。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReminderTier {
    /// 相对任务基准时间的偏移（**秒**）。负值代表提前。
    pub offset_seconds: i64,
    pub is_enabled: bool,
}

impl ReminderTier {
    pub fn new(offset_seconds: i64) -> Self {
        Self {
            offset_seconds,
            is_enabled: true,
        }
    }
}

/// 默认分级策略：截止前 1 天、截止前 1 小时、到点。
///
/// 这三档对应三种不同的决策时机：前一天决定"要不要今天做"，
/// 前一小时决定"现在得动手了"，到点则是最后的提醒。
pub fn default_deadline_tiers() -> Vec<ReminderTier> {
    vec![
        ReminderTier::new(-24 * 3600),
        ReminderTier::new(-3600),
        ReminderTier::new(0),
    ]
}

/// 非截止型任务的默认策略：只在到点提醒一次。
///
/// 刻意不给时间点任务加"提前一小时"：用户说的"3 点开会"就是 3 点，
/// 提前响反而会让人以为时间到了。
pub fn default_point_tiers() -> Vec<ReminderTier> {
    vec![ReminderTier::new(0)]
}

/// 根据时间类型选择默认档位。
pub fn default_tiers_for(kind: TimeKind) -> Vec<ReminderTier> {
    match kind {
        TimeKind::BeforeDeadline => default_deadline_tiers(),
        TimeKind::AtTime | TimeKind::Recurring => default_point_tiers(),
        // 全天任务没有明确的时刻。默认取当天早上 9 点提醒，
        // 由调用方通过 base_time 传入当天的 09:00 来实现。
        TimeKind::AllDay => default_point_tiers(),
    }
}

/// 从任务的基准时间推导出所有提醒的触发时刻，按时间升序。
///
/// # 参数
/// - `tiers`：提醒档位
/// - `base`：任务的时间基准（`due_at` 或 `deadline_at`），带时区偏移
///
/// # 返回
/// 按触发时刻升序排列的 (触发时刻, 档位) 列表。已禁用的档位会被过滤掉。
///
/// 返回**自有**的档位而非引用：`ReminderTier` 只有两个整数字段，克隆成本
/// 可忽略，但换来调用方不必处理生命周期 —— 否则调用方连传一个临时构造的
/// 档位数组都会被借用检查拒绝。
///
/// # 为什么排序在这里做
/// 调度器需要"最早的那个触发点"来决定睡多久。若调用方每次自己排序，
/// 一旦漏排就会导致调度器睡过头而错过提醒 —— 这种 bug 很难复现。
/// 把排序收在产生数据的函数里，调用方就不可能忘。
pub fn fire_times(
    tiers: &[ReminderTier],
    base: DateTime<FixedOffset>,
) -> Vec<(DateTime<FixedOffset>, ReminderTier)> {
    // 偏移量的合理上界：±10 年。
    //
    // 必须夹住，而不是直接交给 `checked_add_signed`：chrono 在做
    // `DateTime + Duration` 时**会先 panic** 而不是返回 None（它的
    // `checked_add_signed` 内部仍会调用可能 panic 的加法）。
    // 也就是说，一个损坏的设置值（例如 JSON 里的 9223372036854775807）
    // 足以让调度器崩溃 —— 而调度器崩溃等于所有提醒失效。
    //
    // 10 年对提醒而言已经远超任何合理需求，夹在这里不会损失任何真实功能。
    const MAX_OFFSET_SECONDS: i64 = 10 * 365 * 24 * 3600;

    let mut out: Vec<(DateTime<FixedOffset>, ReminderTier)> = tiers
        .iter()
        .filter(|t| t.is_enabled)
        .filter_map(|t| {
            let clamped = t
                .offset_seconds
                .clamp(-MAX_OFFSET_SECONDS, MAX_OFFSET_SECONDS);
            // 单位是秒：这样「10 秒后提醒我」才能精确工作
            let at = base.checked_add_signed(Duration::seconds(clamped))?;
            Some((at, t.clone()))
        })
        .collect();

    out.sort_by_key(|(at, _)| *at);
    out
}

/// 把"全天任务"的基准时刻规整到当天指定钟点。
///
/// 全天任务在数据库里存的是 `YYYY-MM-DDT00:00:00`，直接用它做提醒基准
/// 会导致提醒在午夜弹出。这里统一挪到用户设定的钟点（默认 09:00）。
pub fn all_day_base(
    date: chrono::NaiveDate,
    hour: u32,
    minute: u32,
    offset: FixedOffset,
) -> Option<DateTime<FixedOffset>> {
    let naive = NaiveDateTime::new(date, chrono::NaiveTime::from_hms_opt(hour, minute, 0)?);
    naive.and_local_timezone(offset).single()
}

/// 判断某个提醒是否已经错过（触发时刻早于 `now`）。
///
/// 用于启动时补发。单独成函数是为了让"错过"的判定标准只有一处定义 ——
/// 若各处自己写 `at < now`，边界条件（恰好等于）迟早会不一致。
pub fn is_missed(at: DateTime<FixedOffset>, now: DateTime<FixedOffset>) -> bool {
    at < now
}

/// 从任务上取时间基准字符串。
pub fn anchor_string<'a>(
    kind: TimeKind,
    due_at: Option<&'a str>,
    deadline_at: Option<&'a str>,
) -> Option<&'a str> {
    match kind.anchor_field() {
        AnchorField::DueAt => due_at,
        AnchorField::DeadlineAt => deadline_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Datelike` 提供 DateTime::year()。trait 方法必须显式导入才能调用。
    use chrono::{Datelike, TimeZone};

    fn at(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    #[test]
    fn default_tiers_match_spec() {
        let tiers = default_deadline_tiers();
        let offsets: Vec<i64> = tiers.iter().map(|t| t.offset_seconds).collect();
        // 提示词要求：截止前 1 天、前 1 小时、到点
        assert_eq!(offsets, vec![-86400, -3600, 0]);
    }

    /// 按时间类型分发默认档位。
    ///
    /// 单独测这个分发函数而不是只测它调用的两个函数：它是个 match，
    /// 漏配一个分支的表现是"某类任务完全没有提醒"，而且不会有任何报错。
    #[test]
    fn default_tiers_dispatch_covers_every_kind() {
        // 截止型：三级分级提醒
        let deadline = default_tiers_for(TimeKind::BeforeDeadline);
        assert_eq!(deadline.len(), 3);
        assert_eq!(
            deadline
                .iter()
                .map(|t| t.offset_seconds)
                .collect::<Vec<_>>(),
            vec![-86400, -3600, 0]
        );

        // 其余三类：只到点提醒一次。
        // 刻意不给时间点任务加"提前一小时"：用户说"3 点开会"就是 3 点，
        // 提前响反而会让人以为时间到了。
        for kind in [TimeKind::AtTime, TimeKind::Recurring, TimeKind::AllDay] {
            let tiers = default_tiers_for(kind);
            assert_eq!(tiers.len(), 1, "{kind:?} 应只有到点提醒");
            assert_eq!(tiers[0].offset_seconds, 0, "{kind:?} 应为到点提醒");
        }
    }

    /// 档位一律默认为启用 —— 若默认关闭，用户会得到"有档位但从不提醒"。
    #[test]
    fn default_tiers_are_all_enabled() {
        for kind in [
            TimeKind::BeforeDeadline,
            TimeKind::AtTime,
            TimeKind::Recurring,
            TimeKind::AllDay,
        ] {
            for tier in default_tiers_for(kind) {
                assert!(
                    tier.is_enabled,
                    "{kind:?} 的档位 {} 默认为关闭状态",
                    tier.offset_seconds
                );
            }
        }
    }

    /// 极端的偏移量必须被安全地夹住，而不是让 chrono panic。
    ///
    /// 这是由一个失败的测试发现的真实缺陷：`DateTime + Duration` 在 chrono 里
    /// **会先 panic** 而不是返回 None，因此一个损坏的设置值
    /// （例如 JSON 里被篡改成一个巨大整数）足以让调度器崩溃 ——
    /// 而调度器崩溃等于所有提醒全部失效。
    #[test]
    fn extreme_offsets_are_clamped_not_panicking() {
        let base = at("2026-09-29T18:00:00+08:00");

        // 这些值若直接参与日期加法都会 panic
        for extreme in [i64::MAX, i64::MIN, 9_000_000_000_000_000] {
            let tiers = vec![ReminderTier::new(extreme)];
            // 不 panic 本身就是这个测试要断言的核心
            let times = fire_times(&tiers, base);
            assert_eq!(times.len(), 1, "极端偏移应被夹住而不是被丢弃或崩溃");
        }

        // 夹住之后的时刻仍应是有意义的日期（而不是 1970 或某次溢出结果）
        let times = fire_times(&[ReminderTier::new(i64::MAX)], base);
        let at = times[0].0;
        assert!(
            at.year() < base.year() + 11,
            "正向极端偏移应被夹到约 10 年内，实际 {}",
            at
        );
    }

    /// 被夹住之后的排序仍然正确。
    #[test]
    fn clamping_preserves_ordering() {
        let base = at("2026-09-29T18:00:00+08:00");
        let tiers = vec![
            ReminderTier::new(i64::MAX),
            ReminderTier::new(-3600),
            ReminderTier::new(0),
        ];
        let times = fire_times(&tiers, base);
        for w in times.windows(2) {
            assert!(w[0].0 <= w[1].0, "夹住之后仍必须升序");
        }
        // 提前 60 分钟的排最前，极端值被夹到 10 年后排最后
        assert_eq!(times[0].0, at("2026-09-29T17:00:00+08:00"));
    }

    /// 偏移为 0 必须精确等于基准时刻本身。
    #[test]
    fn zero_offset_equals_base() {
        let base = at("2026-09-29T18:00:00+08:00");
        assert_eq!(fire_times(&[ReminderTier::new(0)], base)[0].0, base);
    }

    #[test]
    fn fire_times_are_sorted_ascending() {
        let base = at("2026-09-29T18:00:00+08:00");
        // 必须先绑定：直接传 default_deadline_tiers() 会让临时值在语句结束时
        // 被丢弃，而返回值里持有对它的引用。
        let tiers = default_deadline_tiers();
        let times = fire_times(&tiers, base);

        assert_eq!(times.len(), 3);
        assert_eq!(times[0].0, at("2026-09-28T18:00:00+08:00"));
        assert_eq!(times[1].0, at("2026-09-29T17:00:00+08:00"));
        assert_eq!(times[2].0, at("2026-09-29T18:00:00+08:00"));

        // 升序，供调度器直接取第一个
        for w in times.windows(2) {
            assert!(w[0].0 <= w[1].0, "触发时刻应升序排列");
        }
    }

    /// 档位的输入顺序不应影响输出顺序 —— 调度器依赖这一点。
    #[test]
    fn fire_times_sorted_even_if_tiers_unordered() {
        let base = at("2026-09-29T18:00:00+08:00");
        let scrambled = vec![
            ReminderTier::new(0),
            ReminderTier::new(-86400),
            ReminderTier::new(-3600),
        ];
        let times = fire_times(&scrambled, base);
        assert_eq!(times[0].0, at("2026-09-28T18:00:00+08:00"));
        assert_eq!(times[2].0, at("2026-09-29T18:00:00+08:00"));
    }

    #[test]
    fn disabled_tiers_are_excluded() {
        let base = at("2026-09-29T18:00:00+08:00");
        let mut tiers = default_deadline_tiers();
        tiers[0].is_enabled = false;

        let times = fire_times(&tiers, base);
        assert_eq!(times.len(), 2, "禁用的档位不应产生触发时刻");
        assert!(times
            .iter()
            .all(|(t, _)| *t != at("2026-09-28T18:00:00+08:00")));
    }

    /// 跨日提醒必须算对日期。提前一天的提醒会落到前一天，这是最容易出错的地方。
    #[test]
    fn one_day_before_crosses_date_correctly() {
        // 截止时间是 10 月 1 日 00:30，提前一天应为 9 月 30 日 00:30
        let base = at("2026-10-01T00:30:00+08:00");
        let times = fire_times(&[ReminderTier::new(-86400)], base);
        assert_eq!(times[0].0, at("2026-09-30T00:30:00+08:00"));
    }

    /// 跨月边界。
    #[test]
    fn reminder_crosses_month_boundary() {
        let base = at("2026-10-01T08:00:00+08:00");
        let times = fire_times(&[ReminderTier::new(-3600)], base);
        assert_eq!(times[0].0, at("2026-10-01T07:00:00+08:00"));

        let base2 = at("2026-10-01T00:30:00+08:00");
        let times2 = fire_times(&[ReminderTier::new(-3600)], base2);
        assert_eq!(times2[0].0, at("2026-09-30T23:30:00+08:00"));
    }

    #[test]
    fn all_day_base_lands_on_given_clock() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let offset = FixedOffset::east_opt(8 * 3600).unwrap();
        let base = all_day_base(date, 9, 0, offset).unwrap();
        assert_eq!(base.to_rfc3339(), "2026-09-29T09:00:00+08:00");
    }

    /// 全天任务若直接用午夜做基准，提醒会在半夜弹出 —— 这是必须避免的。
    #[test]
    fn all_day_base_is_not_midnight() {
        let date = chrono::NaiveDate::from_ymd_opt(2026, 9, 29).unwrap();
        let offset = FixedOffset::east_opt(8 * 3600).unwrap();
        let base = all_day_base(date, 9, 0, offset).unwrap();
        assert_ne!(base.time(), chrono::NaiveTime::MIN);
        assert_eq!(base.time().format("%H:%M").to_string(), "09:00");
    }

    #[test]
    fn missed_detection_uses_strictly_before() {
        let now = at("2026-09-29T12:00:00+08:00");
        assert!(is_missed(at("2026-09-29T11:59:00+08:00"), now));
        // 恰好等于不算错过：那一瞬间刚到，应该正常弹出而不是报"错过"
        assert!(!is_missed(now, now));
        assert!(!is_missed(at("2026-09-29T12:01:00+08:00"), now));
    }

    #[test]
    fn anchor_string_picks_correct_field() {
        assert_eq!(
            anchor_string(TimeKind::BeforeDeadline, Some("d"), Some("dl")),
            Some("dl")
        );
        assert_eq!(
            anchor_string(TimeKind::AtTime, Some("d"), Some("dl")),
            Some("d")
        );
        assert_eq!(anchor_string(TimeKind::AtTime, None, Some("dl")), None);
    }

    /// 时区偏移必须被保留，不能因为加减时间而丢失。
    #[test]
    fn offset_is_preserved() {
        let base = at("2026-09-29T18:00:00+08:00");
        let times = fire_times(&default_deadline_tiers(), base);
        for (t, _) in times {
            assert_eq!(
                t.offset().local_minus_utc(),
                8 * 3600,
                "触发时刻丢失了原时区偏移"
            );
        }
    }

    /// 不同时区的同一时刻应算出相同的绝对触发瞬间。
    #[test]
    fn different_offsets_yield_same_instant() {
        let shanghai = at("2026-09-29T18:00:00+08:00");
        let london = at("2026-09-29T10:00:00+00:00");
        // 两者是同一瞬间
        assert_eq!(shanghai.timestamp(), london.timestamp());

        let a = fire_times(&[ReminderTier::new(-3600)], shanghai)[0].0;
        let b = fire_times(&[ReminderTier::new(-3600)], london)[0].0;
        assert_eq!(a.timestamp(), b.timestamp(), "同一瞬间的提醒时刻应一致");
    }

    #[test]
    fn negative_and_positive_offsets_both_work() {
        let base = at("2026-09-29T18:00:00+08:00");
        // 正偏移表示"到点之后再提醒"。单位是秒，5 分钟 = 300 秒。
        let times = fire_times(&[ReminderTier::new(300)], base);
        assert_eq!(times[0].0, at("2026-09-29T18:05:00+08:00"));

        // 秒级偏移必须精确工作 —— 这是「10秒后提醒我」能生效的前提
        let secs = fire_times(&[ReminderTier::new(10)], base);
        assert_eq!(secs[0].0, at("2026-09-29T18:00:10+08:00"));
    }

    #[test]
    fn empty_tiers_yield_no_times() {
        let base = at("2026-09-29T18:00:00+08:00");
        assert!(fire_times(&[], base).is_empty());
    }

    /// 使用 chrono 的 TimeZone trait 构造固定偏移时刻，验证工具函数本身正确。
    #[test]
    fn fixed_offset_construction_is_consistent() {
        let offset = FixedOffset::east_opt(8 * 3600).unwrap();
        let dt = offset.with_ymd_and_hms(2026, 9, 29, 18, 0, 0).unwrap();
        assert_eq!(dt, at("2026-09-29T18:00:00+08:00"));
    }
}
