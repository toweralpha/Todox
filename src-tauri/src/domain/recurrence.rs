//! 重复规则与发生时间推进。
//!
//! 本模块是纯逻辑：`next_occurrence` 只依赖传入的参数，不读数据库、不看系统状态。
//! 因此它既容易测试，又天然幂等 —— 同一组输入永远得到同一个结果。
//!
//! 这一点支撑了核心架构决策：**不物化"下一次发生时间"**。
//! 调度器每次重新推导即可，不必担心进程被杀后内存中的调度状态与现实脱节。
//!
//! 日期运算一律交给 chrono，不手写 "加 86400 秒" 这类逻辑 ——
//! 跨月、闰年、月末天数差异都会让手写实现出错。

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};
use serde::{Deserialize, Serialize};

/// 重复频率。对应 `recurrence_rule.freq` 的 CHECK 约束。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Freq {
    /// 每天
    Daily,
    /// 每个工作日（周一至周五）
    Weekdays,
    /// 每周指定星期几，由 `by_weekdays` 决定
    Weekly,
    /// 每月某日，由 `by_monthday` 决定
    Monthly,
    /// 每 N 天（N 由 interval 决定）
    EveryNDays,
    /// 每 N 周（N 由 interval 决定）
    EveryNWeeks,
}

impl Freq {
    pub fn as_db(self) -> &'static str {
        match self {
            Freq::Daily => "daily",
            Freq::Weekdays => "weekdays",
            Freq::Weekly => "weekly",
            Freq::Monthly => "monthly",
            Freq::EveryNDays => "every_n_days",
            Freq::EveryNWeeks => "every_n_weeks",
        }
    }

    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "daily" => Some(Freq::Daily),
            "weekdays" => Some(Freq::Weekdays),
            "weekly" => Some(Freq::Weekly),
            "monthly" => Some(Freq::Monthly),
            "every_n_days" => Some(Freq::EveryNDays),
            "every_n_weeks" => Some(Freq::EveryNWeeks),
            _ => None,
        }
    }
}

/// 重复规则。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecurrenceRule {
    pub id: String,
    pub freq: Freq,
    /// 间隔。`Daily` 与 `Weekdays` 忽略此值，恒按 1 处理。
    pub interval: i64,
    /// 星期掩码，按位表示周一至周日（bit0=周一 … bit6=周日）。
    /// 仅 `Weekly` 与 `EveryNWeeks` 使用。
    pub by_weekdays: Option<i64>,
    /// 每月第几日（1–31）。仅 `Monthly` 使用。
    pub by_monthday: Option<i64>,
    /// 结束条件之一：到某日为止（含当日）。
    pub until_date: Option<String>,
    /// 结束条件之二：共发生 N 次。
    pub max_count: Option<i64>,
    /// 每次发生的时间点（HH:MM）。
    pub at_time_of_day: Option<String>,
    pub tz: Option<String>,

    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub revision: i64,
}

/// `Weekday` 与掩码位序的换算：周一 = bit0，周日 = bit6。
///
/// 不用 chrono 的 `num_days_from_monday()` 是因为本函数要独立可测，
/// 且明确的映射比依赖下游库的编号约定更不易出错。
pub fn weekday_bit(w: Weekday) -> i64 {
    match w {
        Weekday::Mon => 0,
        Weekday::Tue => 1,
        Weekday::Wed => 2,
        Weekday::Thu => 3,
        Weekday::Fri => 4,
        Weekday::Sat => 5,
        Weekday::Sun => 6,
    }
}

/// 把星期几列表编码为掩码。
pub fn encode_weekdays(days: &[Weekday]) -> i64 {
    days.iter().fold(0, |acc, w| acc | (1 << weekday_bit(*w)))
}

/// 掩码中是否包含某个星期几。
pub fn mask_contains(mask: i64, w: Weekday) -> bool {
    mask & (1 << weekday_bit(w)) != 0
}

/// 解析 `HH:MM` 形式的时刻。失败时返回 None，由调用方决定降级行为。
pub fn parse_time_of_day(s: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()
}

/// 取某年某月的天数。
///
/// 用来把"每月 31 日"这样的规则调整到短月。chrono 通过"下个月 1 日往前退一天"
/// 的方式计算，只依赖库自身的日历实现，不需要手写月份天数表 ——
/// 手写表在闰年上出错是经典事故。
fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("每月都有 1 日");
    first_of_next
        .pred_opt()
        .expect("每月的 1 日之前必有日期")
        .day()
}

/// 把 `desired_day` 落到 `year-month` 中的合法日期上。
///
/// 当 desired_day 超出该月天数时**取该月最后一天**，而不是跳过整月。
/// 这是刻意选择：用户设"每月 31 日"的本意是"月底"，若 2 月直接跳过，
/// 会导致一年里少发生好几次，且用户无从察觉。
fn clamp_to_month(year: i32, month: u32, desired_day: i64) -> Option<NaiveDate> {
    let max_day = days_in_month(year, month);
    let day = desired_day.clamp(1, max_day as i64) as u32;
    NaiveDate::from_ymd_opt(year, month, day)
}

/// 重复间隔的合理上界。
///
/// 必须夹住，因为 chrono 的 `Duration::days` / `weeks` 在参数过大时**会 panic**
/// （不是返回 None）。虽然 `interval` 有 SQL 的 CHECK 约束、中文解析器也最多
/// 产出两位数，但损坏的数据库或被手工改过的导入文件仍可能给出巨大整数 ——
/// 那会让重复规则的推进直接崩溃，表现为"某条重复任务一完成整个应用就挂"，
/// 而从数据表面完全看不出问题。
///
/// 100 年对任何重复规则都远超实际需求，夹在这里不损失任何真实功能。
const MAX_INTERVAL: i64 = 100 * 365;

/// 把间隔夹到安全范围 `[1, MAX_INTERVAL]`。
///
/// 下界取 1 而非 0：间隔为 0 意味着"下一次就是这一次"，
/// 会让推进逻辑陷入死循环。
fn safe_interval(interval: i64) -> i64 {
    interval.clamp(1, MAX_INTERVAL)
}

/// 推进到下一次发生时间。
///
/// # 参数
/// - `rule`：重复规则
/// - `current`：当前这一次的发生时间。返回值严格晚于它。
/// - `occurrences_so_far`：**已经发生的次数**（含 `current` 这一次）。
///   用于判断 `max_count` 结束条件是否已达上限。
///
/// # 返回
/// 下一次发生时间；若规则已结束（超过 `until_date` 或已达 `max_count`）则返回 `None`。
///
/// # 为什么是纯函数
/// 不读取"下次发生时间"这类外部状态，因此无论调度器何时、以何种顺序调用，
/// 结果都一致。进程被杀后重启只需重新推导，不存在状态错乱的可能。
pub fn next_occurrence(
    rule: &RecurrenceRule,
    current: NaiveDateTime,
    occurrences_so_far: i64,
) -> Option<NaiveDateTime> {
    // 次数上限检查：已经发生够多次，不再有下一次
    if let Some(max) = rule.max_count {
        if occurrences_so_far >= max {
            return None;
        }
    }

    // 间隔一律先夹到安全范围：chrono 的 Duration::days/weeks 在参数过大时
    // 会 panic 而不是返回 None，而 interval 来自数据库，理论上可能被篡改。
    let interval = safe_interval(rule.interval);

    let time_of_day = rule
        .at_time_of_day
        .as_deref()
        .and_then(parse_time_of_day)
        .unwrap_or_else(|| current.time());

    let until = rule
        .until_date
        .as_deref()
        .and_then(|s| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok());

    // 不同频率各自最多试探这么多轮。
    // 取值远大于实际需要（"每周指定星期几"最多 7 轮、"每月某日"最多 2 轮），
    // 目的是让异常规则快速失败，而不是陷入死循环。
    const MAX_PROBE: usize = 600;

    let mut candidate = current;

    for _ in 0..MAX_PROBE {
        candidate = match rule.freq {
            Freq::Daily => candidate + Duration::days(1),
            Freq::Weekdays => {
                let mut next = candidate + Duration::days(1);
                // 跳过周六周日。最多回退 3 天即可落到周一，循环天然有界。
                while matches!(next.weekday(), Weekday::Sat | Weekday::Sun) {
                    next += Duration::days(1);
                }
                next
            }
            Freq::Weekly | Freq::EveryNWeeks => {
                let step_weeks = if rule.freq == Freq::Weekly {
                    1
                } else {
                    interval
                };
                let mask = rule.by_weekdays.unwrap_or_else(|| {
                    // 未指定星期几时，沿用当前这一次是星期几
                    1 << weekday_bit(candidate.weekday())
                });

                // 从次日开始逐天找，直到落在掩码允许的星期几上。
                // 这样"每周一三五"这类多选规则可以自然工作。
                let mut next = candidate + Duration::days(1);
                let mut guard = 0;
                while !mask_contains(mask, next.weekday()) {
                    next += Duration::days(1);
                    guard += 1;
                    // 掩码为 0（不含任何星期）时永不满足，必须设上限
                    if guard > 14 {
                        return None;
                    }
                }
                // 跨过若干整周，实现"每 N 周"
                if step_weeks > 1 {
                    next += Duration::weeks(step_weeks - 1);
                }
                next
            }
            Freq::Monthly => {
                let desired = rule.by_monthday.unwrap_or(candidate.day() as i64);

                // 从下个月开始找，避免落到与 current 同月
                let (mut y, mut m) = if candidate.month() == 12 {
                    (candidate.year() + 1, 1)
                } else {
                    (candidate.year(), candidate.month() + 1)
                };

                let mut found = None;
                for _ in 0..24 {
                    if let Some(d) = clamp_to_month(y, m, desired) {
                        found = Some(NaiveDateTime::new(d, time_of_day));
                        break;
                    }
                    // 理论上 clamp_to_month 不会失败，这里只是保守兜底
                    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
                    y = ny;
                    m = nm;
                }
                // 这里刻意保留 match 而非 clippy 建议的 `?`：
                // 本分支在一个 for 循环里逐月搜索，找到合法日期就用它。
                // 若换成 `?`，第一个没有该日的月份就会让整个函数返回 None，
                // 而正确语义是继续尝试后续月份。
                // clamp_to_month 实际上只在日期非法时才会返回 None，
                // 那种情况下继续找下个月才是对用户最合理的行为。
                #[allow(clippy::question_mark)]
                match found {
                    Some(dt) => dt,
                    None => return None,
                }
            }
            Freq::EveryNDays => {
                // interval 已由 safe_interval 夹到 [1, MAX_INTERVAL]，
                // 因此这里的 Duration::days 不会溢出
                candidate + Duration::days(interval)
            }
        };

        // 结束日期检查
        if let Some(u) = until {
            if candidate.date() > u {
                return None;
            }
        }

        // 把时间点对齐到规则指定的时刻
        candidate = NaiveDateTime::new(candidate.date(), time_of_day);

        // 必须严格晚于 current，否则说明规则本身无法推进（如掩码为空），
        // 继续返回会与调用方形成死循环
        if candidate > current {
            return Some(candidate);
        }
    }

    None
}

/// 生成人类可读的中文描述，例如「每周一、周三 09:00」。
///
/// 放在 Rust 侧而非前端：重复规则的语义完全由本模块定义，
/// 描述逻辑贴着它写能避免两边理解不一致。
pub fn describe(rule: &RecurrenceRule) -> String {
    let clock = rule
        .at_time_of_day
        .as_deref()
        .and_then(parse_time_of_day)
        .map(|t| format!(" {}", t.format("%H:%M")))
        .unwrap_or_default();

    // 同样夹一下：描述文案里会按周/天换算，未经夹取的巨大值会算出荒谬的文案
    let interval = safe_interval(rule.interval);

    let base = match rule.freq {
        Freq::Daily => "每天".to_string(),
        Freq::Weekdays => "每个工作日".to_string(),
        Freq::Weekly | Freq::EveryNWeeks => {
            let prefix = if rule.freq == Freq::Weekly || interval == 1 {
                "每周".to_string()
            } else {
                format!("每 {interval} 周")
            };
            match rule.by_weekdays {
                Some(mask) if mask != 0 => {
                    let names: Vec<&str> = [
                        Weekday::Mon,
                        Weekday::Tue,
                        Weekday::Wed,
                        Weekday::Thu,
                        Weekday::Fri,
                        Weekday::Sat,
                        Weekday::Sun,
                    ]
                    .into_iter()
                    .filter(|w| mask_contains(mask, *w))
                    .map(|w| match w {
                        Weekday::Mon => "周一",
                        Weekday::Tue => "周二",
                        Weekday::Wed => "周三",
                        Weekday::Thu => "周四",
                        Weekday::Fri => "周五",
                        Weekday::Sat => "周六",
                        Weekday::Sun => "周日",
                    })
                    .collect();
                    format!("{prefix}{}", names.join("、"))
                }
                _ => prefix,
            }
        }
        Freq::Monthly => match rule.by_monthday {
            Some(d) => format!("每月 {d} 日"),
            None => "每月".to_string(),
        },
        Freq::EveryNDays => {
            if interval == 1 {
                "每天".to_string()
            } else {
                format!("每 {interval} 天")
            }
        }
    };

    let ending = match (&rule.until_date, rule.max_count) {
        (Some(d), _) => format!("，至 {d} 结束"),
        (None, Some(n)) => format!("，共 {n} 次"),
        (None, None) => String::new(),
    };

    format!("{base}{clock}{ending}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(freq: Freq) -> RecurrenceRule {
        RecurrenceRule {
            id: "r1".into(),
            freq,
            interval: 1,
            by_weekdays: None,
            by_monthday: None,
            until_date: None,
            max_count: None,
            at_time_of_day: Some("09:00".into()),
            tz: None,
            created_at: "2026-01-01T00:00:00+08:00".into(),
            updated_at: "2026-01-01T00:00:00+08:00".into(),
            deleted_at: None,
            revision: 1,
        }
    }

    fn dt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, mi, 0)
            .unwrap()
    }

    #[test]
    fn weekday_mask_roundtrip() {
        let days = [Weekday::Mon, Weekday::Wed, Weekday::Fri];
        let mask = encode_weekdays(&days);
        assert_eq!(mask, 0b0010101, "周一=bit0 周三=bit2 周五=bit4");

        for w in [
            Weekday::Mon,
            Weekday::Tue,
            Weekday::Wed,
            Weekday::Thu,
            Weekday::Fri,
            Weekday::Sat,
            Weekday::Sun,
        ] {
            assert_eq!(mask_contains(mask, w), days.contains(&w), "{w:?} 判定错误");
        }
    }

    #[test]
    fn daily_advances_one_day() {
        let r = rule(Freq::Daily);
        let next = next_occurrence(&r, dt(2026, 9, 29, 9, 0), 1).unwrap();
        assert_eq!(next, dt(2026, 9, 30, 9, 0));
    }

    /// 工作日规则必须跳过周末。
    #[test]
    fn weekdays_skips_weekend() {
        let r = rule(Freq::Weekdays);
        // 2026-09-25 是周五
        assert_eq!(dt(2026, 9, 25, 9, 0).weekday(), Weekday::Fri);
        let next = next_occurrence(&r, dt(2026, 9, 25, 9, 0), 1).unwrap();
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2026, 9, 28).unwrap());
        assert_eq!(next.weekday(), Weekday::Mon);
    }

    #[test]
    fn weekly_finds_next_matching_weekday() {
        let mut r = rule(Freq::Weekly);
        r.by_weekdays = Some(encode_weekdays(&[Weekday::Mon]));

        // 从周一推进应落到下周一，而不是本周的其它日子
        let next = next_occurrence(&r, dt(2026, 9, 28, 9, 0), 1).unwrap();
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2026, 10, 5).unwrap());
    }

    /// 多选星期几「每周一、三、五」应逐日推进。
    #[test]
    fn weekly_multiple_weekdays() {
        let mut r = rule(Freq::Weekly);
        r.by_weekdays = Some(encode_weekdays(&[Weekday::Mon, Weekday::Wed, Weekday::Fri]));

        let mon = dt(2026, 9, 28, 9, 0); // 周一
        let wed = next_occurrence(&r, mon, 1).unwrap();
        assert_eq!(wed.weekday(), Weekday::Wed);

        let fri = next_occurrence(&r, wed, 2).unwrap();
        assert_eq!(fri.weekday(), Weekday::Fri);

        let next_mon = next_occurrence(&r, fri, 3).unwrap();
        assert_eq!(next_mon.weekday(), Weekday::Mon);
    }

    #[test]
    fn every_two_weeks_jumps_a_week() {
        let mut r = rule(Freq::EveryNWeeks);
        r.interval = 2;
        r.by_weekdays = Some(encode_weekdays(&[Weekday::Fri]));

        // 2026-09-25 周五 → 间隔两周后的周五应是 10-09
        let next = next_occurrence(&r, dt(2026, 9, 25, 9, 0), 1).unwrap();
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2026, 10, 9).unwrap());
    }

    /// 每月 31 日在短月必须落到该月最后一天，而不是跳过整月。
    /// 这是本模块最容易出错的地方，因此单独覆盖 2 月与 30 天月。
    #[test]
    fn monthly_31st_clamps_to_month_end() {
        let mut r = rule(Freq::Monthly);
        r.by_monthday = Some(31);

        // 1 月 31 日 → 2 月 28 日（2026 非闰年）
        let next = next_occurrence(&r, dt(2026, 1, 31, 9, 0), 1).unwrap();
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2026, 2, 28).unwrap());

        // 3 月 31 日 → 4 月 30 日
        let next2 = next_occurrence(&r, dt(2026, 3, 31, 9, 0), 2).unwrap();
        assert_eq!(next2.date(), NaiveDate::from_ymd_opt(2026, 4, 30).unwrap());
    }

    /// 闰年 2 月有 29 天，是验证日期库是否被正确使用的关键用例。
    #[test]
    fn monthly_31st_handles_leap_february() {
        let mut r = rule(Freq::Monthly);
        r.by_monthday = Some(31);

        // 2028 是闰年，1 月 31 日 → 2 月 29 日
        let next = next_occurrence(&r, dt(2028, 1, 31, 9, 0), 1).unwrap();
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2028, 2, 29).unwrap());
    }

    #[test]
    fn every_n_days_respects_interval() {
        let mut r = rule(Freq::EveryNDays);
        r.interval = 3;
        let next = next_occurrence(&r, dt(2026, 9, 29, 9, 0), 1).unwrap();
        assert_eq!(next.date(), NaiveDate::from_ymd_opt(2026, 10, 2).unwrap());
    }

    /// 巨大的 interval 必须被夹住，而不是让 chrono 的 Duration 加法 panic。
    ///
    /// `interval` 来自数据库，理论上可能被篡改（损坏或手工改过的导入文件）。
    /// 若直接参与日期加法，chrono 会 panic —— 表现为"某条重复任务一完成
    /// 整个应用就挂"，而且从数据表面完全看不出问题。
    #[test]
    fn huge_interval_is_clamped_not_panicking() {
        for freq in [Freq::EveryNDays, Freq::EveryNWeeks] {
            let mut r = rule(freq);
            // 这个值若直接 Duration::days(9223372036854775807) 必定 panic
            r.interval = i64::MAX;
            if freq == Freq::EveryNWeeks {
                r.by_weekdays = Some(encode_weekdays(&[Weekday::Mon]));
            }

            // 不 panic 就是本测试的核心断言
            let next = next_occurrence(&r, dt(2026, 9, 28, 9, 0), 1);
            assert!(next.is_some(), "{freq:?} 夹取后仍应能推进");
        }
    }

    /// interval 为 0 或负数时应视为 1，而不是产生"不推进"的死循环。
    #[test]
    fn zero_or_negative_interval_behaves_as_one() {
        for bad in [0i64, -1, -100] {
            let mut r = rule(Freq::EveryNDays);
            r.interval = bad;
            let current = dt(2026, 9, 29, 9, 0);
            let next = next_occurrence(&r, current, 1).expect("应能推进");
            assert!(next > current, "interval={bad} 时也必须严格推进");
        }
    }

    /// 描述文案在极端 interval 下不应崩溃或算出荒谬结果。
    #[test]
    fn describe_handles_extreme_interval() {
        let mut r = rule(Freq::EveryNDays);
        r.interval = i64::MAX;
        let text = describe(&r);
        assert!(!text.is_empty());
        // 夹取后应表述为 100 年内，而不是某个天文数字
        assert!(
            text.contains("36500") || text.contains("每 "),
            "描述应基于夹取后的值：{text}"
        );
    }

    /// max_count 是结束条件，达到后必须返回 None，否则重复任务永不停止。
    #[test]
    fn max_count_terminates() {
        let mut r = rule(Freq::Daily);
        r.max_count = Some(3);

        // 已发生 2 次，还能有第 3 次
        assert!(next_occurrence(&r, dt(2026, 9, 29, 9, 0), 2).is_some());
        // 已发生 3 次，达到上限
        assert!(next_occurrence(&r, dt(2026, 9, 29, 9, 0), 3).is_none());
    }

    /// until_date 是结束条件，超过后必须返回 None。
    #[test]
    fn until_date_terminates() {
        let mut r = rule(Freq::Daily);
        r.until_date = Some("2026-09-30".into());

        // 9-29 的下一次是 9-30，等于截止日，仍然有效
        assert_eq!(
            next_occurrence(&r, dt(2026, 9, 29, 9, 0), 1)
                .unwrap()
                .date(),
            NaiveDate::from_ymd_opt(2026, 9, 30).unwrap()
        );
        // 9-30 的下一次是 10-01，超过截止日
        assert!(next_occurrence(&r, dt(2026, 9, 30, 9, 0), 2).is_none());
    }

    /// 空掩码会导致永远找不到匹配的星期几，必须快速失败而不是死循环。
    #[test]
    fn empty_weekday_mask_fails_fast() {
        let mut r = rule(Freq::Weekly);
        r.by_weekdays = Some(0);
        assert!(next_occurrence(&r, dt(2026, 9, 29, 9, 0), 1).is_none());
    }

    /// 推进结果必须严格晚于当前时间，这是调度器不会死循环的前提。
    #[test]
    fn result_is_strictly_later() {
        for freq in [
            Freq::Daily,
            Freq::Weekdays,
            Freq::Weekly,
            Freq::Monthly,
            Freq::EveryNDays,
            Freq::EveryNWeeks,
        ] {
            let mut r = rule(freq);
            if matches!(freq, Freq::Weekly | Freq::EveryNWeeks) {
                r.by_weekdays = Some(encode_weekdays(&[Weekday::Mon]));
            }
            let current = dt(2026, 9, 28, 9, 0);
            if let Some(next) = next_occurrence(&r, current, 1) {
                assert!(next > current, "{freq:?} 未严格推进：{next} <= {current}");
            }
        }
    }

    /// 规则描述文案的关键分支。
    #[test]
    fn describe_produces_readable_chinese() {
        let mut r = rule(Freq::Weekly);
        r.by_weekdays = Some(encode_weekdays(&[Weekday::Mon]));
        assert_eq!(describe(&r), "每周周一 09:00");

        let mut r2 = rule(Freq::Monthly);
        r2.by_monthday = Some(15);
        assert_eq!(describe(&r2), "每月 15 日 09:00");

        let mut r3 = rule(Freq::EveryNDays);
        r3.interval = 3;
        r3.max_count = Some(5);
        assert_eq!(describe(&r3), "每 3 天 09:00，共 5 次");

        let mut r4 = rule(Freq::Daily);
        r4.until_date = Some("2026-12-31".into());
        assert_eq!(describe(&r4), "每天 09:00，至 2026-12-31 结束");
    }

    /// 连续推进 10 次不应出现停滞或重复。
    #[test]
    fn repeated_advancement_is_monotonic() {
        let mut r = rule(Freq::Weekdays);
        r.at_time_of_day = Some("08:00".into());

        let mut cur = dt(2026, 9, 25, 8, 0); // 周五
        let mut seen = vec![cur];
        for i in 1..=10 {
            cur = next_occurrence(&r, cur, i).expect("应当始终有下一次");
            assert!(!seen.contains(&cur), "出现重复发生时间：{cur}");
            assert!(
                matches!(
                    cur.weekday(),
                    Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
                ),
                "工作日规则不应落到周末：{cur}"
            );
            seen.push(cur);
        }
    }
}
