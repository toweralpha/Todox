//! 时间与标识符工具。
//!
//! 关键决策：**时间戳一律带本地时区偏移**（例如 `2026-09-29T14:30:00+08:00`），
//! 而不是转成 UTC 存储。
//!
//! 理由：待办应用里"今天"这个概念完全由用户所在时区决定。若存 UTC，那么
//! "今天要做的事"在跨时区或夏令时切换时会出现整天错位，而这个错误极难复现。
//! 存储带偏移的本地时间，"今天"就永远等于用户看到的那一天。
//! 额外的收益是直接读写数据库时时间可读，调试成本远低于一堆 epoch 整数。
//!
//! 代价是需要明确：跨时区同步的场景下，同一时刻的不同偏移表示需要归一化比较。
//! 这一点由 chrono 的 `DateTime` 比较自动处理，因此不构成实际问题。

use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, SecondsFormat, Weekday};

/// 相对时长（秒）的绝对上限：10 年。
///
/// # 为什么必须有这个上限
///
/// chrono 的 `NaiveDateTime + TimeDelta` 在结果越界时**会 panic**
/// （不是返回 `None`）。而用户输入里的数字完全无界：输入
/// `13800138000小时后回电` 会让偏移量达到约 4.9e13 秒，直接触发
/// `'NaiveDateTime + TimeDelta' overflowed`。
///
/// 更严重的是：前端在用户**输入停顿约 180ms 后**就会调用 `parse_input`
/// 做实时预览，因此用户还没点保存、只是在打字或粘贴，进程就可能已经消失。
/// 而 release 配置是 `panic = "abort"`，没有 unwind —— 托盘常驻与所有提醒
/// 会一起消失，且不留任何提示。
///
/// 10 年对任何待办场景都远超实际需求，因此夹在这里不损失任何真实功能。
///
/// **所有从用户输入推导出的时长都必须经 [`clamp_duration_seconds`] 收敛。**
/// 不要在各自分支里另设上限 —— 那正是这个 bug 的成因：
/// 提醒偏移与重复间隔都设了上限，偏偏时长入口漏了。
pub const MAX_DURATION_SECONDS: i64 = 10 * 365 * 24 * 3600;

/// 把时长（秒）夹到 `[-MAX_DURATION_SECONDS, MAX_DURATION_SECONDS]`。
pub fn clamp_duration_seconds(seconds: i64) -> i64 {
    seconds.clamp(-MAX_DURATION_SECONDS, MAX_DURATION_SECONDS)
}

/// 当前时刻，RFC3339 格式并保留本地时区偏移。
///
/// `SecondsFormat::Secs` 而非 `AutoSi`：精确到秒足够（提醒的粒度不会更细），
/// 且定长输出在文本排序与肉眼比对时都更方便。
pub fn now_rfc3339() -> String {
    Local::now().to_rfc3339_opts(SecondsFormat::Secs, false)
}

/// 生成一个新的 UUID v4，用作实体主键。
///
/// 用 UUID 而非自增整数是云同步的前提：自增 ID 在多设备上必然碰撞。
pub fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 解析 RFC3339 时间戳。
///
/// 返回 `DateTime<FixedOffset>` 而非 `DateTime<Local>`：解析结果保留原始偏移，
/// 因此对历史数据的展示不会因为用户后来换了时区而改变含义。
pub fn parse_rfc3339(s: &str) -> Result<DateTime<FixedOffset>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(s)
}

/// 把日期渲染成中文直觉表达，例如「今天」「明天」「周三」「9月30日」。
///
/// 只比较**日期**而不比较时刻：待办应用里"明天"指的是日历上的下一天，
/// 与当前是几点无关。用 `signed_duration_since` 减去时间部分再比天数，
/// 而不是比较 `day()` 的差值 —— 后者在跨月（9月30日→10月1日）时会算出负数。
pub fn human_date(target: NaiveDate, today: NaiveDate) -> String {
    let diff = target.signed_duration_since(today).num_days();

    match diff {
        0 => "今天".to_string(),
        1 => "明天".to_string(),
        2 => "后天".to_string(),
        -1 => "昨天".to_string(),
        // 未来一周内用星期几，更贴合"这周还要干什么"的思维
        3..=6 => weekday_cn(target.weekday()),
        _ => {
            if target.year() == today.year() {
                format!("{}月{}日", target.month(), target.day())
            } else {
                format!("{}年{}月{}日", target.year(), target.month(), target.day())
            }
        }
    }
}

/// 星期几的中文简称。
pub fn weekday_cn(w: Weekday) -> String {
    match w {
        Weekday::Mon => "周一",
        Weekday::Tue => "周二",
        Weekday::Wed => "周三",
        Weekday::Thu => "周四",
        Weekday::Fri => "周五",
        Weekday::Sat => "周六",
        Weekday::Sun => "周日",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_parseable_and_ordered() {
        let a = now_rfc3339();
        let b = now_rfc3339();

        // 必须能被自己解析回来，否则写进数据库的时间就是一串死文本
        parse_rfc3339(&a).expect("生成的时间戳应当可解析");

        // RFC3339 的定长格式保证字典序等于时间序，
        // 这正是 SQLite 用 TEXT 存时间还能正确 ORDER BY 的原因。
        assert!(a <= b, "时间戳的字典序应随时间递增：{a} vs {b}");
    }

    #[test]
    fn now_carries_timezone_offset_not_zulu() {
        let s = now_rfc3339();
        // 结尾为 'Z' 代表 UTC。刻意不使用 UTC 存储，见模块文档。
        assert!(!s.ends_with('Z'), "时间戳应带本地偏移而非 UTC：{s}");
        assert!(
            s.contains('+') || s[10..].contains('-'),
            "时间戳应含时区偏移：{s}"
        );
    }

    #[test]
    fn ids_are_unique_and_v4() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b, "连续生成的 ID 不应相同");
        assert_eq!(a.len(), 36, "应为标准 UUID 字符串长度：{a}");
        assert_eq!(a.chars().filter(|c| *c == '-').count(), 4);
    }

    /// 字典序 == 时间序，这是 SQLite 用 TEXT 排时间的前提。
    /// 不同偏移的时间戳若混在同一列，字典序会失效 —— 因此本测试固定同偏移。
    #[test]
    fn lexicographic_order_matches_chronological_order() {
        let mut v = [
            "2026-09-29T14:30:00+08:00",
            "2026-09-29T09:05:00+08:00",
            "2026-12-01T00:00:00+08:00",
            "2026-01-15T23:59:00+08:00",
        ];
        v.sort();

        let parsed: Vec<_> = v.iter().map(|s| parse_rfc3339(s).unwrap()).collect();
        for pair in parsed.windows(2) {
            assert!(pair[0] <= pair[1], "排序后顺序应保持时间递增");
        }
    }

    // ===================== 中文日期标签 =====================

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    #[test]
    fn human_date_covers_near_days() {
        let today = d(2026, 9, 29);
        assert_eq!(human_date(d(2026, 9, 29), today), "今天");
        assert_eq!(human_date(d(2026, 9, 30), today), "明天");
        assert_eq!(human_date(d(2026, 10, 1), today), "后天");
        assert_eq!(human_date(d(2026, 9, 28), today), "昨天");
    }

    /// 一周内用星期几。2026-10-02 是周五。
    #[test]
    fn human_date_uses_weekday_within_a_week() {
        let today = d(2026, 9, 29); // 周二
        assert_eq!(human_date(d(2026, 10, 2), today), "周五");
    }

    /// 跨月必须算对 —— 这是用日期相减而非比较 day() 的原因。
    #[test]
    fn human_date_handles_month_boundary() {
        assert_eq!(human_date(d(2026, 10, 1), d(2026, 9, 30)), "明天");
        assert_eq!(human_date(d(2026, 10, 2), d(2026, 9, 30)), "后天");

        // 9 月 30 日 到 10 月 20 日 超过一周，应给出月日
        assert_eq!(human_date(d(2026, 10, 20), d(2026, 9, 30)), "10月20日");
    }

    /// 跨年时必须带上年份，否则"1月5日"会有歧义。
    ///
    /// 注意要选**超过一周**之后的日期：一周内会显示为星期几
    /// （「周二」这种更贴合"这几天要做什么"的思维），那是刻意的产品行为。
    #[test]
    fn human_date_includes_year_across_years() {
        // 2026-12-30 到 2027-01-20 相距 21 天，超出"一周内用星期几"的范围
        assert_eq!(human_date(d(2027, 1, 20), d(2026, 12, 30)), "2027年1月20日");
    }

    /// 一周内显示为星期几，这是刻意的产品决策而非缺陷。
    #[test]
    fn human_date_within_a_week_shows_weekday_even_across_years() {
        // 2026-12-30 是周三，2027-01-05 距其 6 天 → 落在"一周内"区间
        assert_eq!(human_date(d(2027, 1, 5), d(2026, 12, 30)), "周二");
    }

    /// 闰年的 2 月 29 日必须存在且能正确渲染。
    #[test]
    fn human_date_handles_leap_day() {
        assert_eq!(human_date(d(2028, 2, 29), d(2028, 2, 28)), "明天");
        assert_eq!(human_date(d(2028, 3, 15), d(2028, 2, 28)), "3月15日");
    }

    #[test]
    fn weekday_cn_maps_all_days() {
        assert_eq!(weekday_cn(chrono::Weekday::Mon), "周一");
        assert_eq!(weekday_cn(chrono::Weekday::Sun), "周日");
    }
}
