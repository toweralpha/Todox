//! 解析器的单元测试。
//!
//! 所有测试都传入**固定的 `now`**，不依赖系统当前时间。
//! 否则测试会随真实日期漂移 —— 今天通过、下个月失败，这种失败极难排查。

use super::*;
use chrono::{NaiveDate, Timelike};

/// 固定的参考时刻：2026-09-29（周二）14:00。
fn fixed_now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 9, 29)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap()
}

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn dt(y: i32, m: u32, day: u32, h: u32, mi: u32) -> NaiveDateTime {
    d(y, m, day).and_hms_opt(h, mi, 0).unwrap()
}

// ===================== 相对日期 =====================

#[test]
fn parses_relative_days() {
    let now = fixed_now();
    let today = d(2026, 9, 29);

    assert_eq!(parse("今天 开会", now).date, Some(today));
    assert_eq!(parse("明天 开会", now).date, Some(d(2026, 9, 30)));
    assert_eq!(parse("后天 开会", now).date, Some(d(2026, 10, 1)));
    assert_eq!(parse("大后天 开会", now).date, Some(d(2026, 10, 2)));
    assert_eq!(parse("昨天 开会", now).date, Some(d(2026, 9, 28)));
}

/// 「大后天」必须优先于「后天」匹配，否则会退化成后天。
#[test]
fn big_day_after_tomorrow_is_not_shadowed() {
    let p = parse("大后天 交材料", fixed_now());
    assert_eq!(
        p.date,
        Some(d(2026, 10, 2)),
        "大后天应是 3 天后而不是 2 天后"
    );
    assert_eq!(p.title, "交材料");
}

#[test]
fn parses_n_days_later_with_arabic_and_chinese_numerals() {
    let now = fixed_now();
    assert_eq!(parse("3天后 还书", now).date, Some(d(2026, 10, 2)));
    assert_eq!(parse("三天后 还书", now).date, Some(d(2026, 10, 2)));
    assert_eq!(parse("5 天后 还书", now).date, Some(d(2026, 10, 4)));
    assert_eq!(parse("十天后 还书", now).date, Some(d(2026, 10, 9)));
}

#[test]
fn parses_weekday_names() {
    let now = fixed_now(); // 2026-09-29 周二
                           // 周三 = 明天
    assert_eq!(parse("周三 开会", now).date, Some(d(2026, 9, 30)));
    // 周一：本周一已过，应指下周一
    assert_eq!(parse("周一 开会", now).date, Some(d(2026, 10, 5)));
    // 周二即今天，但"最近的周二"应指下周二
    assert_eq!(parse("周二 开会", now).date, Some(d(2026, 10, 6)));
    assert_eq!(parse("星期日 开会", now).date, Some(d(2026, 10, 4)));
    assert_eq!(parse("礼拜五 开会", now).date, Some(d(2026, 10, 2)));
}

#[test]
fn parses_next_week_prefix() {
    let now = fixed_now(); // 2026-09-29 周二，本周一为 09-28
                           // 「下周X」以**自然周**为基准：本周一 09-28 + 7 天 = 10-05 即下周一。
                           // （10-12 是"下下周一"，不是"下周一"。）
    assert_eq!(parse("下周一 开会", now).date, Some(d(2026, 10, 5)));
    // 下周三 = 下周一 + 2 天 = 10-07，虽然本周三（09-30）更早，
    // 但"下周三"明确指下一周，不应退化成本周三。
    assert_eq!(parse("下周三 开会", now).date, Some(d(2026, 10, 7)));
    // 本周五 = 10-02，尚未到，仍是本周
    assert_eq!(parse("本周五 开会", now).date, Some(d(2026, 10, 2)));
}

/// 「下周」与「本周」必须区分开，否则用户会漏掉一整周。
#[test]
fn next_week_differs_from_this_week() {
    let now = fixed_now(); // 周二
    let next_wed = parse("下周三 开会", now).date.unwrap();
    let this_wed = parse("本周三 开会", now).date.unwrap();
    assert_eq!(this_wed, d(2026, 9, 30));
    assert_eq!(next_wed, d(2026, 10, 7));
    assert_eq!(
        (next_wed - this_wed).num_days(),
        7,
        "下周同一天应恰好比本周晚 7 天"
    );
}

// ===================== 绝对日期 =====================

#[test]
fn parses_absolute_month_day() {
    let now = fixed_now();
    assert_eq!(parse("9月30日 交材料", now).date, Some(d(2026, 9, 30)));
    assert_eq!(parse("9月30号 交材料", now).date, Some(d(2026, 9, 30)));
    assert_eq!(parse("10月15日 交材料", now).date, Some(d(2026, 10, 15)));
    assert_eq!(parse("十五号 交材料", now).date, Some(d(2026, 10, 15)));
}

/// 未写年份且该日期今年已过时，应落到明年 —— 12 月说「1月5日」显然指明年。
#[test]
fn past_month_day_rolls_to_next_year() {
    let now = NaiveDate::from_ymd_opt(2026, 12, 20)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    let p = parse("1月5日 交年报", now);
    assert_eq!(p.date, Some(d(2027, 1, 5)), "已过的月日应落到明年");

    // 而尚未到来的月日仍留在今年
    let p2 = parse("12月25日 过圣诞", now);
    assert_eq!(p2.date, Some(d(2026, 12, 25)));
}

#[test]
fn parses_absolute_date_with_year() {
    let now = fixed_now();
    assert_eq!(parse("2027年3月8日 交材料", now).date, Some(d(2027, 3, 8)));
    assert_eq!(parse("2027-03-08 交材料", now).date, Some(d(2027, 3, 8)));
}

// ===================== 时刻 =====================

#[test]
fn parses_clock_formats() {
    let now = fixed_now();
    assert_eq!(parse("14:30 开会", now).at, Some(dt(2026, 9, 29, 14, 30)));
    assert_eq!(parse("9:00 开会", now).at, Some(dt(2026, 9, 30, 9, 0)));
    assert_eq!(
        parse("明天 14:30 开会", now).at,
        Some(dt(2026, 9, 30, 14, 30))
    );
}

/// 单独的时刻若今天已过，应指明天；尚未到的时刻仍是今天。
///
/// 注意「下午3点30分」这种情况：今天 14:00 看它，15:30 还没到，
/// 因此**属于今天**是正确的。这正是"已过才顺延"的行为。
#[test]
fn past_clock_rolls_to_tomorrow() {
    let now = fixed_now(); // 14:00
                           // 09:00 已过 → 明天
    assert_eq!(parse("9:00 开会", now).at, Some(dt(2026, 9, 30, 9, 0)));
    // 18:00 未到 → 今天
    assert_eq!(parse("18:00 开会", now).at, Some(dt(2026, 9, 29, 18, 0)));
    // 15:30 未到 → 今天
    assert_eq!(
        parse("下午3点30分 开会", now).at,
        Some(dt(2026, 9, 29, 15, 30))
    );
}

/// 「明天」+时刻的组合不受"已过顺延"影响：日期是明确的。
#[test]
fn explicit_date_disables_rollover() {
    let now = fixed_now(); // 14:00
                           // 上午9点已过，但既然明确说了明天，就应当是明天的 9:00
    assert_eq!(
        parse("明天早上9点 开会", now).at,
        Some(dt(2026, 9, 30, 9, 0))
    );
}

#[test]
fn parses_chinese_period_prefixes() {
    let now = fixed_now();
    let cases = [
        ("下午3点 开会", 15, 0),
        ("上午9点 开会", 9, 0),
        ("早上8点 开会", 8, 0),
        ("凌晨1点 开会", 1, 0),
        ("中午12点 吃饭", 12, 0),
        ("晚上8点 加班", 20, 0),
        ("傍晚6点 下班", 18, 0),
        ("深夜11点 睡觉", 23, 0),
    ];
    for (input, h, mi) in cases {
        let p = parse(input, now);
        // 这些时刻的日期取决于是否已过，这里只断言时分正确
        let at = p.at.unwrap_or_else(|| panic!("{input} 未解析出时刻"));
        assert_eq!(
            (at.time().hour(), at.time().minute()),
            (h, mi),
            "{input} 解析错误：{at}"
        );
    }
}

#[test]
fn parses_minutes_and_half() {
    let now = fixed_now(); // 14:00

    // 15:30 尚未到 → 今天
    assert_eq!(
        parse("下午3点30分 开会", now).at,
        Some(dt(2026, 9, 29, 15, 30))
    );
    // 09:15 与 08:30 都已过 → 明天
    assert_eq!(parse("9点15分 开会", now).at, Some(dt(2026, 9, 30, 9, 15)));
    assert_eq!(parse("八点半 开会", now).at, Some(dt(2026, 9, 30, 8, 30)));
    // 「分」可省略
    assert_eq!(parse("9点15 开会", now).at, Some(dt(2026, 9, 30, 9, 15)));
    // 「点钟」这种赘字写法
    assert_eq!(parse("9点钟 开会", now).at, Some(dt(2026, 9, 30, 9, 0)));
}

/// 「晚上12点」是次日零点，不是中午 12 点。
#[test]
fn midnight_edge_cases() {
    let now = fixed_now();
    let p = parse("晚上12点 睡觉", now);
    assert_eq!(p.at.map(|a| a.time().hour()), Some(0), "晚上12点应为 0 点");
}

// ===================== 重复规则 =====================

#[test]
fn parses_daily() {
    let now = fixed_now();
    for input in ["每天 8:00 吃维生素", "每日 8:00 吃维生素"] {
        let p = parse(input, now);
        let r = p.recurrence.expect("应解析出重复规则");
        assert_eq!(r.freq, Freq::Daily);
        assert_eq!(r.at_time_of_day.as_deref(), Some("08:00"));
        assert_eq!(p.title, "吃维生素");
    }
}

#[test]
fn parses_weekdays() {
    let p = parse("每个工作日 9:00 打卡", fixed_now());
    let r = p.recurrence.expect("应解析出重复规则");
    assert_eq!(r.freq, Freq::Weekdays);
    assert_eq!(p.title, "打卡");
}

#[test]
fn parses_weekly_single_day() {
    let p = parse("每周一早上9点 提交周报", fixed_now());
    let r = p.recurrence.expect("应解析出重复规则");
    assert_eq!(r.freq, Freq::Weekly);
    assert_eq!(r.by_weekdays, Some(encode_weekdays(&[Weekday::Mon])));
    assert_eq!(r.at_time_of_day.as_deref(), Some("09:00"));
    assert_eq!(p.title, "提交周报");
}

#[test]
fn parses_weekly_multiple_days() {
    let p = parse("每周一三五 9:00 锻炼", fixed_now());
    let r = p.recurrence.expect("应解析出重复规则");
    assert_eq!(r.freq, Freq::Weekly);
    assert_eq!(
        r.by_weekdays,
        Some(encode_weekdays(&[Weekday::Mon, Weekday::Wed, Weekday::Fri]))
    );
    assert_eq!(p.title, "锻炼");
}

#[test]
fn parses_every_n_days() {
    let now = fixed_now();
    let p = parse("每3天 浇水", now);
    let r = p.recurrence.expect("应解析出重复规则");
    assert_eq!(r.freq, Freq::EveryNDays);
    assert_eq!(r.interval, 3);
    assert_eq!(p.title, "浇水");

    let p2 = parse("每三天 浇水", now);
    assert_eq!(p2.recurrence.unwrap().interval, 3);
}

#[test]
fn parses_every_n_weeks() {
    let p = parse("每2周 周五 复盘", fixed_now());
    let r = p.recurrence.expect("应解析出重复规则");
    assert_eq!(r.freq, Freq::EveryNWeeks);
    assert_eq!(r.interval, 2);
}

#[test]
fn parses_monthly() {
    let now = fixed_now();
    for input in ["每月15日 交房租", "每月15号 交房租"] {
        let p = parse(input, now);
        let r = p.recurrence.expect("应解析出重复规则");
        assert_eq!(r.freq, Freq::Monthly);
        assert_eq!(r.by_monthday, Some(15));
        assert_eq!(p.title, "交房租");
    }
}

#[test]
fn parses_recurrence_with_time() {
    let p = parse("每月1日下午2点 交报表", fixed_now());
    let r = p.recurrence.expect("应解析出重复规则");
    assert_eq!(r.freq, Freq::Monthly);
    assert_eq!(r.by_monthday, Some(1));
    assert_eq!(r.at_time_of_day.as_deref(), Some("14:00"));
    assert_eq!(p.title, "交报表");
}

// ===================== 标题剥离 =====================

#[test]
fn title_strips_time_expression() {
    let now = fixed_now();
    let cases = [
        ("明天下午3点 交房租", "交房租"),
        ("每周一早上9点 提交周报", "提交周报"),
        ("3天后 还书", "还书"),
        ("每天 8:00 吃维生素", "吃维生素"),
        ("2月14日 18:00 前 订餐厅", "订餐厅"),
        ("下周一 开会", "开会"),
        ("每月15日 交房租", "交房租"),
    ];
    for (input, expected_title) in cases {
        let p = parse(input, now);
        assert_eq!(p.title, expected_title, "「{input}」的标题剥离错误");
    }
}

/// 标题里与时间表达无关的数字不能被误剥离。
#[test]
fn title_keeps_unrelated_numbers() {
    let p = parse("买3个苹果", fixed_now());
    assert_eq!(p.title, "买3个苹果");
    assert!(p.at.is_none(), "「3个」不应被当成时间");
    assert!(p.date.is_none());
    assert!(p.recurrence.is_none());
}

#[test]
fn unrelated_numbers_are_not_parsed_as_time() {
    let now = fixed_now();
    for input in [
        "我买了3个苹果",
        "5次会议",
        "读了2本书",
        "约了3个人",
        "开了10分钟会",
    ] {
        let p = parse(input, now);
        assert!(
            p.at.is_none() && p.date.is_none() && p.offset_seconds.is_none(),
            "「{input}」被误判为包含时间：{p:?}"
        );
        assert_eq!(p.title, input, "「{input}」的标题被误改");
    }
}

/// 解析不出时间时，标题必须完整保留原始输入 —— 绝不能丢内容。
#[test]
fn failed_parse_preserves_full_input() {
    let p = parse("整理书桌", fixed_now());
    assert_eq!(p.title, "整理书桌");
    assert!(p.at.is_none());
    assert!(p.matched_text.is_none());
}

#[test]
fn empty_input_is_handled() {
    let now = fixed_now();
    for input in ["", "   ", "\t"] {
        let p = parse(input, now);
        assert!(p.at.is_none() && p.date.is_none() && p.recurrence.is_none());
        assert!(p.title.is_empty(), "空白输入应得到空标题：{:?}", p.title);
    }
}

/// 只输入时间表达时，标题会为空 —— 调用方需要处理这种情况（用时间标签当标题）。
/// 这里只断言解析结果本身，不替调用方做决定。
#[test]
fn time_only_input_yields_empty_title() {
    let p = parse("明天下午3点", fixed_now());
    assert!(p.at.is_some());
    assert!(p.title.is_empty());
    assert!(p.matched_text.is_some());
}

// ===================== 相对时长 =====================

#[test]
fn parses_relative_duration() {
    let now = fixed_now();
    // 单位是**秒**
    let p = parse("3小时后 取快递", now);
    assert_eq!(p.offset_seconds, Some(3 * 3600));
    assert_eq!(p.title, "取快递");

    let p2 = parse("30分钟后 关火", now);
    assert_eq!(p2.offset_seconds, Some(30 * 60));
    assert_eq!(p2.title, "关火");
}

/// 「N 天前」被解析成**日期**而不是偏移量。
///
/// 这是刻意的：`parse_date` 先于 `parse_duration` 运行，"2天前"落在它能处理的
/// 范围内（它支持正负天数），于是得到一个明确的日期。两种表示都说得通，
/// 但日期解析给出的结果更具体，所以用它。
#[test]
fn n_days_ago_is_parsed_as_a_date_not_an_offset() {
    let now = fixed_now();
    let p = parse("2天前 发生了什么", now);

    assert_eq!(
        p.date,
        Some(NaiveDate::from_ymd_opt(2026, 9, 27).unwrap()),
        "2 天前应是 09-27"
    );
    assert_eq!(p.title, "发生了什么");
    assert!(p.offset_seconds.is_none(), "已由日期表达，不需要再用偏移量");
}

/// 秒级时长是用户验证提醒功能时最常用的输入。
///
/// 这条测试对应一个真实缺陷：早期实现以**分钟**为内部单位，
/// 且时长单位表里根本没有「秒」，于是「10秒后提醒我」既解析不出时间，
/// 也不会安排任何提醒 —— 用户看到的是"输入了时间却毫无反馈"。
#[test]
fn parses_second_level_duration() {
    let now = fixed_now();

    let p = parse("10秒后 提醒我", now);
    assert_eq!(
        p.offset_seconds,
        Some(10),
        "「10秒后」必须被解析成 10 秒，而不是被截断成 0 或完全不识别"
    );
    assert_eq!(p.title, "提醒我");

    let p2 = parse("30秒钟后 检查", now);
    assert_eq!(p2.offset_seconds, Some(30));

    let p3 = parse("45秒后 测试", now);
    assert_eq!(p3.offset_seconds, Some(45));

    // 中文数字也应支持
    let p4 = parse("十秒后 提醒", now);
    assert_eq!(p4.offset_seconds, Some(10));
}

/// 秒级时长必须能通过完整解析产出可用的绝对时间。
///
/// 解析出 offset 还不够 —— 调用方要把它换算成绝对时刻，
/// 若那一步用了 minutes()，10 秒仍然会被截断成 0。
#[test]
fn second_level_offset_yields_future_time() {
    let now = fixed_now();
    let p = parse("10秒后 提醒我", now);

    let offset = p.offset_seconds.expect("应解析出偏移");
    let target = now + chrono::Duration::seconds(offset);

    assert!(target > now, "目标时刻必须在未来");
    assert_eq!(
        (target - now).num_seconds(),
        10,
        "与当前时刻的差必须恰好是 10 秒"
    );
}

// ===================== 跨年与闰年 =====================

/// 12 月 31 日的「明天」必须是次年 1 月 1 日。
#[test]
fn crosses_year_boundary() {
    let now = NaiveDate::from_ymd_opt(2026, 12, 31)
        .unwrap()
        .and_hms_opt(20, 0, 0)
        .unwrap();
    let p = parse("明天 元旦", now);
    assert_eq!(p.date, Some(d(2027, 1, 1)));
    assert_eq!(p.title, "元旦");
}

/// 闰年 2 月 28 日的「明天」是 2 月 29 日。
#[test]
fn handles_leap_year() {
    let now = NaiveDate::from_ymd_opt(2028, 2, 28)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    let p = parse("明天 交材料", now);
    assert_eq!(p.date, Some(d(2028, 2, 29)), "2028 是闰年，应有 2 月 29 日");
}

/// 非闰年的 2 月 28 日「明天」是 3 月 1 日。
#[test]
fn handles_non_leap_year() {
    let now = NaiveDate::from_ymd_opt(2026, 2, 28)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    let p = parse("明天 交材料", now);
    assert_eq!(p.date, Some(d(2026, 3, 1)));
}

#[test]
fn crosses_month_boundary() {
    let now = NaiveDate::from_ymd_opt(2026, 9, 30)
        .unwrap()
        .and_hms_opt(10, 0, 0)
        .unwrap();
    assert_eq!(parse("明天 开会", now).date, Some(d(2026, 10, 1)));
    assert_eq!(parse("3天后 开会", now).date, Some(d(2026, 10, 3)));
}

// ===================== 中文数字 =====================

#[test]
fn parses_chinese_numerals() {
    assert_eq!(parse_number("一"), Some(1));
    assert_eq!(parse_number("九"), Some(9));
    assert_eq!(parse_number("十"), Some(10));
    assert_eq!(parse_number("十一"), Some(11));
    assert_eq!(parse_number("十五"), Some(15));
    assert_eq!(parse_number("二十"), Some(20));
    assert_eq!(parse_number("二十三"), Some(23));
    assert_eq!(parse_number("三十一"), Some(31));
    assert_eq!(parse_number("两"), Some(2));
}

#[test]
fn rejects_unsupported_chinese_numerals() {
    // 超过两位数的中文数字不解析。待办场景里几乎不会出现，
    // 而支持它会让扫描逻辑显著复杂化。
    assert_eq!(parse_number("一百"), None);
    assert_eq!(parse_number(""), None);
}

#[test]
fn scan_number_handles_both_scripts() {
    let chars: Vec<char> = "15日".chars().collect();
    assert_eq!(scan_number(&chars), (15, 2));

    let chars2: Vec<char> = "十五日".chars().collect();
    assert_eq!(scan_number(&chars2), (15, 2));

    let chars3: Vec<char> = "日".chars().collect();
    assert_eq!(scan_number(&chars3), (0, 0));
}

// ===================== matched_text =====================

#[test]
fn matched_text_records_original_expression() {
    let p = parse("明天下午3点 交房租", fixed_now());
    let m = p.matched_text.expect("应记录命中原文");
    assert!(
        m.contains("明天") && m.contains("下午3点"),
        "命中原文应包含完整时间表达，实际：{m}"
    );
}

#[test]
fn matched_text_is_none_without_time() {
    let p = parse("整理书桌", fixed_now());
    assert!(p.matched_text.is_none());
}
