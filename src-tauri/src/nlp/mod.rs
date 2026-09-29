//! 中文自然语言时间解析。
//!
//! # 设计原则：宁可少解析，不要错解析
//!
//! 解析错误会让任务出现在错误的时间，用户可能因此错事；而解析不出来只是
//! 落到收件箱，用户手工设一次时间即可。两者的代价不对称，因此本模块对
//! 任何拿不准的表达一律选择"不解析"：
//!
//! - 不引入正则库：中文时间表达的边界靠字符扫描判断已经足够，且本项目对
//!   二进制体积与内存有硬性要求。
//! - 不猜测无单位数字：`我买了3个苹果` 里的 `3` 不会被当成时间。
//! - 解析失败时 `title` 必定包含原始输入，绝不丢失用户写下的内容。

use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime, Weekday};

use crate::domain::recurrence::{encode_weekdays, Freq, RecurrenceRule};
use crate::domain::time::{new_id, now_rfc3339};

/// 解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedTime {
    /// 具体时间点。能算出绝对时刻的表达填这里。
    pub at: Option<NaiveDateTime>,
    /// 仅日期。用于"全天"类表达（如只写了「明天」）。
    pub date: Option<NaiveDate>,
    /// 相对当前时刻的偏移（**秒**）。用于「3小时后」「10秒后」这类纯时长表达。
    ///
    /// 单位是秒而不是分钟：待办应用里"10 秒后"这种输入确实是常见需求
    /// （用户想快速验证提醒是否工作），若以分钟为单位，10 秒会被截断成 0，
    /// 表现是"输入了时间但任务落在收件箱里"。
    pub offset_seconds: Option<i64>,
    /// 解析出的重复规则。
    pub recurrence: Option<RecurrenceRule>,
    /// 去掉时间表达后剩余的标题。
    pub title: String,
    /// 命中的时间表达原文，用于在界面上展示给用户确认。
    pub matched_text: Option<String>,
}

/// 解析主入口。
///
/// `now` 由调用方传入而非内部取系统时间，这样才能写出**确定性**的单元测试 ——
/// 否则测试会随真实时间漂移，今天通过明天失败。
pub fn parse(input: &str, now: NaiveDateTime) -> ParsedTime {
    let mut s = input.trim().to_string();
    if s.is_empty() {
        return ParsedTime {
            at: None,
            date: None,
            offset_seconds: None,
            recurrence: None,
            title: String::new(),
            matched_text: None,
        };
    }

    let mut consumed: Vec<String> = Vec::new();

    // ---------- 1. 重复规则 ----------
    // 重复表达总在句首（「每周一…」「每天…」），且它决定了后面怎么解读时刻。
    let recurrence = parse_recurrence(&mut s, &mut consumed);

    // ---------- 2. 日期 ----------
    let mut date: Option<NaiveDate> = None;
    if recurrence.is_none() {
        if let Some((d, matched)) = parse_date(&s, now) {
            date = Some(d);
            consumed.push(matched.clone());
            s = strip_first(&s, &matched);
        }
    }

    // ---------- 3. 时长（3小时后）----------
    // 放在时刻之前：两者互斥，而「3小时后」不含冒号或「点」，
    // 不会被时刻扫描捕获，因此顺序只为让剥离逻辑更清晰。
    let mut offset_seconds: Option<i64> = None;
    if recurrence.is_none() {
        // 变量名用 secs：它承载的是**秒**，用 mins 会让后续读代码的人
        // 误以为单位是分钟（这正是"把 10 秒截断成 0"那类 bug 的来源）。
        if let Some((secs, matched)) = parse_duration(&s) {
            offset_seconds = Some(secs);
            consumed.push(matched.clone());
            s = strip_first(&s, &matched);
        }
    }

    // ---------- 4. 时刻 ----------
    // **必须放在日期之后，且要扫描全文而不只是开头**：
    // 「明天下午3点 交房租」里的时刻位于日期之后，
    // 若只认字符串开头就永远找不到它。
    //
    // 这一步**不能**加 `recurrence.is_none()` 条件：
    // parse_recurrence 只把时刻填进了规则，并没有从字符串里删掉它，
    // 因此「每周一早上9点 提交周报」仍需在此把「早上9点」剥离，
    // 否则标题会残留「每周一」这样的碎片。
    let mut time_of_day: Option<NaiveTime> = None;
    if let Some((time, matched, _)) = find_time_of_day(&s) {
        time_of_day = Some(time);
        consumed.push(matched.clone());
        s = strip_first(&s, &matched);
    }

    // 重复规则若指定了时刻，把它回填进规则的 at_time_of_day
    let recurrence = recurrence.map(|mut r| {
        if r.at_time_of_day.is_none() {
            if let Some(t) = time_of_day {
                r.at_time_of_day = Some(t.format("%H:%M").to_string());
            }
        }
        r
    });

    // ---------- 4. 组装 ----------
    // 优先级：有日期 + 有时刻 → 绝对时间点；有日期无时刻 → 只有日期；
    // 只有时长 → 偏移量。
    let (at, final_date) = match (date, time_of_day) {
        (Some(d), Some(t)) => (Some(NaiveDateTime::new(d, t)), Some(d)),
        (Some(d), None) => (None, Some(d)),
        // 只有时刻没日期：指今天。若该时刻已过，则指明天 ——
        // 这是符合直觉的："9:00" 在下午输入时显然指明早。
        (None, Some(t)) => {
            let today = now.date();
            let candidate = NaiveDateTime::new(today, t);
            if candidate > now {
                (Some(candidate), Some(today))
            } else {
                let tomorrow = today + Duration::days(1);
                (Some(NaiveDateTime::new(tomorrow, t)), Some(tomorrow))
            }
        }
        (None, None) => (None, None),
    };

    // 一般情况下，`date` 已通过 `at` 表达了，就不再单独返回，避免调用方
    // 在选择字段时产生歧义。
    //
    // **但重复规则是例外**：`at` 对"今天已过的时刻"会顺延到明天，那个日期
    // 并不是规则真正的首次发生日。以「每周一早上9点」为例，周二解析时
    // `at` 是 09-30（周二），而规则的目标日 `date` 是 10-05（下周一）——
    // 若在这里丢掉 `date`，调用方只能拿到错误的那一天，星期几就全错了。
    // 因此有重复规则时必须保留 `date`。
    let date_out = if at.is_some() && recurrence.is_none() {
        None
    } else {
        final_date
    };

    let matched_text = if consumed.is_empty() {
        None
    } else {
        Some(consumed.join(""))
    };

    ParsedTime {
        at,
        date: date_out,
        offset_seconds,
        recurrence,
        title: clean_title(&s),
        matched_text,
    }
}

// ============================================================================
// 时刻解析
// ============================================================================

/// 在字符串中**任意位置**查找时刻表达，返回 (时刻, 命中原文, 结束字符下标)。
///
/// 之所以要扫描全文而不只看开头：`明天下午3点 交房租` 里的时刻在日期之后。
/// 只认开头的实现在这类最常见的表达上会直接失效。
///
/// **返回命中原文而非仅下标**：早期实现让调用方用长度反推要剥离的片段，
/// 结果一旦"匹配到的东西"与"剥离掉的东西"不一致（时段前缀是否算入、
/// 空白是否算入），就会出现时刻被解析出来却没被剥离的幽灵 bug。
/// 直接返回匹配本身，就消除了这类不一致的可能。
fn find_time_of_day(s: &str) -> Option<(NaiveTime, String, usize)> {
    let chars: Vec<char> = s.chars().collect();

    for i in 0..chars.len() {
        // 只有从"数字"或"时段前缀"开头的位置才可能起一个时刻表达。
        //
        // 中文数字必须一并放行：`parse_time_of_day` 内部支持「八点半」，
        // 但若这里不放行，那个位置根本不会被尝试到，支持等于白写。
        // 早期实现正是漏了这一项，"八点半"这类写法静默失效。
        if !chars[i].is_ascii_digit() && !is_cn_numeral(chars[i]) && !is_period_start(&chars, i) {
            continue;
        }
        // 数字前面紧跟别的数字说明我们落在了一个多位数中间
        // （例如「13点」里从 '3' 开始尝试），跳过以免截出错误数字。
        if i > 0 && chars[i].is_ascii_digit() && chars[i - 1].is_ascii_digit() {
            continue;
        }

        let tail: String = chars[i..].iter().collect();
        if let Some((time, n)) = parse_time_of_day(&tail) {
            let start = find_time_start(&chars, i);

            // 时刻后面可能跟「前」「后」这类修饰（「18:00 前 订餐厅」）。
            // 它们属于时间表达的一部分，必须一并剥离，否则标题里会残留一个「前」。
            //
            // 注意先 trim 再判断：`tail_rest` 往往以空格开头（「 前 订餐厅」），
            // 直接 starts_with('前') 永远匹配不上。
            let tail_rest: String = chars[(i + n).min(chars.len())..].iter().collect();
            let leading_ws = tail_rest.chars().count() - tail_rest.trim_start().chars().count();
            let stripped = tail_rest.trim_start();

            let suffix_len = if stripped.starts_with("之前")
                || stripped.starts_with("以前")
                || stripped.starts_with("之后")
                || stripped.starts_with("以后")
            {
                leading_ws + 2
            } else if stripped.starts_with('前') || stripped.starts_with('后') {
                leading_ws + 1
            } else {
                0
            };

            // 命中原文用与原串相同的下标区间从 `s` 上截取，
            // 避免在多字节字符上切出非法边界。
            let matched = slice_chars(s, start, i + n + suffix_len);
            return Some((time, matched, i + n + suffix_len));
        }
    }

    None
}

/// 判断 `i` 处是否是某个时段前缀的起点。
fn is_period_start(chars: &[char], i: usize) -> bool {
    let tail: String = chars[i..].iter().collect();
    for p in [
        "凌晨", "早上", "上午", "中午", "下午", "傍晚", "晚上", "深夜",
    ] {
        if tail.starts_with(p) {
            return true;
        }
    }
    false
}

/// 把时刻的起始位置向前扩展，涵盖紧邻的时段前缀。
///
/// 查找范围限定在 2 个字符内（最长的时段前缀「凌晨」为 2 字），
/// 避免误吞标题里的字。
fn find_time_start(chars: &[char], i: usize) -> usize {
    let from = i.saturating_sub(2);
    for j in from..i {
        if is_period_start(chars, j) {
            // 确认该前缀恰好结束于 i，而不是更早
            let tail: String = chars[j..i].iter().collect();
            for p in [
                "凌晨", "早上", "上午", "中午", "下午", "傍晚", "晚上", "深夜",
            ] {
                if tail == p {
                    return j;
                }
            }
        }
    }
    i
}

/// 从字符串开头解析时刻，返回 (时刻, 消耗的字符数)。
///
/// 支持形式：`14:30` `9:00` `下午3点` `上午9点` `晚上8点` `中午12点`
/// `早上8点` `凌晨1点` `傍晚6点` `9点15分` `八点半` `下午3点30分`
fn parse_time_of_day(s: &str) -> Option<(NaiveTime, usize)> {
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return None;
    }

    // 时段前缀（下午 / 上午 / 晚上 …）
    let (period, p_len) = match_period(&chars);
    let mut i = p_len;

    // 必须紧跟数字。「下午会议」这类不含数字的表达不应被当成时刻。
    let (hour, h_len) = scan_number(chars.get(i..).unwrap_or(&[]));
    if h_len == 0 || hour > 23 {
        return None;
    }
    i += h_len;

    let has_colon = chars.get(i) == Some(&':');
    let has_dian = matches!(chars.get(i), Some('点') | Some('时'));
    let has_zhong = chars.get(i) == Some(&'钟');

    // 既无分隔符也无时段前缀 → 不是时刻表达。
    // 这一条是防误判的关键：它拦住了「3个苹果」「5次会议」里的裸数字。
    if !has_colon && !has_dian && !has_zhong && period.is_none() {
        return None;
    }

    let mut minute: i64 = 0;
    if has_colon {
        i += 1;
        let (m, m_len) = scan_number(chars.get(i..).unwrap_or(&[]));
        // 冒号后必须是数字，否则「10: 开会」这种会被误判
        if m_len == 0 || m > 59 {
            return None;
        }
        minute = m;
        i += m_len;
    } else if has_dian || has_zhong {
        i += 1;
        // 吃掉「点钟」的「钟」
        if has_dian && chars.get(i) == Some(&'钟') {
            i += 1;
        }

        if chars.get(i) == Some(&'半') {
            minute = 30;
            i += 1;
        } else {
            let (m, m_len) = scan_number(chars.get(i..).unwrap_or(&[]));
            // 「分」是可选的：「9点15」与「9点15分」都合理
            if m_len > 0 && m <= 59 {
                minute = m;
                i += m_len;
                if chars.get(i) == Some(&'分') {
                    i += 1;
                }
            }
        }
    }

    let h = adjust_hour(hour, period);
    Some((NaiveTime::from_hms_opt(h, minute as u32, 0)?, i))
}

/// 匹配时段前缀，返回 (前缀, 字符数)。
fn match_period(chars: &[char]) -> (Option<&'static str>, usize) {
    // 顺序重要：两字前缀必须排在单字之前，否则「凌晨」会被当成「凌」。
    for p in [
        "凌晨", "早上", "上午", "中午", "下午", "傍晚", "晚上", "深夜",
    ] {
        let pc: Vec<char> = p.chars().collect();
        if chars.len() >= pc.len() && chars[..pc.len()] == pc[..] {
            return (Some(p), pc.len());
        }
    }
    (None, 0)
}

/// 把 12 小时制的钟点按中文时段换算成 24 小时制。
fn adjust_hour(hour: i64, period: Option<&str>) -> u32 {
    let h = match period {
        // 下午与晚上：1–11 点加 12。12 点保持不变（中午 12 点即 12:00）。
        Some("下午") | Some("傍晚") => {
            if (1..=11).contains(&hour) {
                hour + 12
            } else {
                hour
            }
        }
        Some("晚上") | Some("深夜") => {
            if (1..=11).contains(&hour) {
                hour + 12
            } else if hour == 12 {
                0
            } else {
                hour
            }
        }
        Some("中午") => {
            if (1..=3).contains(&hour) {
                hour + 12
            } else {
                hour
            }
        }
        // 凌晨 / 早上 / 上午 / 无前缀：保持原值。
        // 「晚上12点」已在上面的分支处理为 0 点。
        _ => hour,
    };
    h.clamp(0, 23) as u32
}

// ============================================================================
// 重复规则解析
// ============================================================================

/// 解析重复表达。成功后从 `s` 中移除该片段。
fn parse_recurrence(s: &mut String, consumed: &mut Vec<String>) -> Option<RecurrenceRule> {
    let base = |freq: Freq, interval: i64| RecurrenceRule {
        id: new_id(),
        freq,
        interval,
        by_weekdays: None,
        by_monthday: None,
        until_date: None,
        max_count: None,
        at_time_of_day: None,
        tz: None,
        created_at: now_rfc3339(),
        updated_at: now_rfc3339(),
        deleted_at: None,
        revision: 1,
    };

    // ---- 每个工作日 ----
    if s.contains("每个工作日") || s.contains("每工作日") {
        let m = if s.contains("每个工作日") {
            "每个工作日"
        } else {
            "每工作日"
        };
        *s = strip_first(s, m);
        consumed.push(m.to_string());
        return Some(base(Freq::Weekdays, 1));
    }

    // ---- 每天 / 每日 ----
    for m in ["每天", "每日"] {
        if s.contains(m) {
            *s = strip_first(s, m);
            consumed.push(m.to_string());
            return Some(base(Freq::Daily, 1));
        }
    }

    // ---- 每周 + 星期几（可多个，如「每周一三五」）----
    if s.contains("每周") || s.contains("每星期") || s.contains("每礼拜") {
        let prefix = if s.contains("每周") {
            "每周"
        } else if s.contains("每星期") {
            "每星期"
        } else {
            "每礼拜"
        };

        let mut after = strip_first_once(s, prefix);
        let mut days: Vec<Weekday> = Vec::new();
        // 累积被消费的星期几原文，用于把完整表达从标题里剥离干净。
        // 逐段累积而不是事后用长度反推：剥离过程会 trim 与压缩空白，
        // 长度关系一旦被破坏，反推出来的片段就是错的（会残留「每周一」）。
        let mut days_text = String::new();

        // 连续读取星期几字符，中间允许「、」「和」「及」
        loop {
            // 先试带前缀的形式（「周一」），再试裸字符（「一」）。
            // 两者都要：用户既可能写「每周一」，也可能写「每周一三五」。
            let (w, n, text) = {
                let (w1, n1) = match_weekday_char(&after);
                if let Some(day) = w1 {
                    (Some(day), n1, slice_chars(&after, 0, n1))
                } else if let Some(day) = match_bare_weekday(&after) {
                    (Some(day), 1, slice_chars(&after, 0, 1))
                } else {
                    (None, 0, String::new())
                }
            };

            match w {
                Some(day) => {
                    if !days.contains(&day) {
                        days.push(day);
                    }
                    days_text.push_str(&text);
                    after = slice_from_chars(&after, n);

                    let sep = match_any_len(&after, &["、", "和", "及", ","]);
                    if sep > 0 {
                        days_text.push_str(&slice_chars(&after, 0, sep));
                        after = slice_from_chars(&after, sep);
                    }
                }
                None => break,
            }
        }

        if !days.is_empty() {
            let mask = encode_weekdays(&days);
            // 命中原文 = 「每周」+ 消费掉的星期几部分
            consumed.push(format!("{prefix}{days_text}"));
            *s = after;

            let mut rule = base(Freq::Weekly, 1);
            rule.by_weekdays = Some(mask);
            return Some(rule);
        }
    }

    // ---- 每 N 周 / 每 N 天 / 每 N 月 ----
    // 注意必须放在「每周」判断之后，否则「每2周」会被「每周」误匹配。
    if let Some(rest) = strip_prefix_any(s, &["每", "隔"]) {
        let (n, n_len) = scan_number_str(&rest);
        if n_len > 0 {
            let unit_part = slice_from_chars(&rest, n_len);
            let (freq, unit_len) = if unit_part.starts_with("周")
                || unit_part.starts_with("星期")
                || unit_part.starts_with("礼拜")
            {
                let l = if unit_part.starts_with("星期") || unit_part.starts_with("礼拜") {
                    2
                } else {
                    1
                };
                (Some(Freq::EveryNWeeks), l)
            } else if unit_part.starts_with("天") || unit_part.starts_with("日") {
                (Some(Freq::EveryNDays), 1)
            } else if unit_part.starts_with("个") {
                // 「每2个月」
                let after_ge = slice_from_chars(&unit_part, 1);
                if after_ge.starts_with("月") {
                    (Some(Freq::Monthly), 2)
                } else {
                    (None, 0)
                }
            } else {
                (None, 0)
            };

            if let Some(f) = freq {
                let total = s.chars().count() - unit_part.chars().count() + unit_len;
                consumed.push(slice_chars(s, 0, total));
                *s = slice_from_chars(&unit_part, unit_len);
                return Some(base(f, n.max(1)));
            }
        }
    }

    // ---- 每月 N 日 / 每月 N 号 ----
    // 前缀按**从长到短**排列：「每月的」→「每个月」→「每月」。
    // 顺序反了虽然也能靠后续校验失败而继续尝试，但那是侥幸而非设计。
    for prefix in ["每月的", "每个月", "每月"] {
        if s.contains(prefix) {
            let after = strip_first_once(s, prefix);
            let (day, d_len) = scan_number_str(&after);
            if d_len > 0 && (1..=31).contains(&day) {
                let rest = slice_from_chars(&after, d_len);
                let unit_len = if rest.starts_with('日') || rest.starts_with('号') {
                    1
                } else {
                    0
                };
                if unit_len > 0 {
                    let total = s.chars().count() - rest.chars().count() + unit_len;
                    consumed.push(slice_chars(s, 0, total));
                    *s = slice_from_chars(&rest, unit_len);

                    let mut rule = base(Freq::Monthly, 1);
                    rule.by_monthday = Some(day);
                    return Some(rule);
                }
            }
        }
    }

    None
}

// ============================================================================
// 日期解析
// ============================================================================

/// 解析日期表达，返回 (日期, 命中的原文)。
fn parse_date(s: &str, now: NaiveDateTime) -> Option<(NaiveDate, String)> {
    let today = now.date();

    // ---- 相对日 ----
    // 顺序重要：多字表达必须排在前面，否则「大后天」会被「后天」抢先匹配。
    for (m, days) in [
        ("大后天", 3i64),
        ("后天", 2),
        ("明天", 1),
        ("明日", 1),
        ("今天", 0),
        ("今日", 0),
        ("昨天", -1),
        ("昨日", -1),
    ] {
        if s.contains(m) {
            return Some((today + Duration::days(days), m.to_string()));
        }
    }

    // ---- N 天后 / N 天前 ----
    if let Some((n, matched, sign)) = scan_relative_days(s) {
        // **必须夹住天数**：`n` 来自用户输入且无界，而
        // `today + Duration::days(n)` 在越界时**会 panic**
        // （release 下 `panic = "abort"` 直接杀进程）。
        // 输入 `99999999天后取货` 就能触发。
        let max_days = crate::domain::time::MAX_DURATION_SECONDS / 86400;
        let days = n.min(max_days).saturating_mul(sign);
        return Some((today + Duration::days(days), matched));
    }

    // ---- 下周X / 这周X / 本周X ----
    for (prefix, week_offset) in [("下周", 1i64), ("这周", 0), ("本周", 0)] {
        if let Some(after) = strip_prefix_any(s, &[prefix]) {
            // 剥掉前缀后剩下的是裸星期字符（如「下周一」→「一」），
            // 因此这里必须用 match_bare_weekday 而不是 match_weekday_char
            if let Some(d) = match_bare_weekday(&after) {
                let target = next_weekday(today, d, week_offset);
                let matched = format!("{prefix}{}", weekday_char(d));
                return Some((target, matched));
            }
        }
    }

    // ---- 周X / 星期X / 礼拜X ----
    if let (Some(d), consumed_len) = match_weekday_char(s) {
        let target = next_weekday(today, d, 0);
        return Some((target, slice_chars(s, 0, consumed_len)));
    }

    // ---- 绝对日期 ----
    if let Some((d, matched)) = parse_absolute_date(s, today) {
        return Some((d, matched));
    }

    // ---- 只有日没有月：十五号 / 30日 ----
    // 必须放在月日解析**之后**，否则它会抢走「9月30日」里的那个 9。
    if let Some((d, matched)) = parse_bare_day(s, today) {
        return Some((d, matched));
    }

    None
}

/// 扫描「N天后」「N天前」，返回 (天数, 原文, 符号)。
///
/// 两处必须小心：
///   1. 数字与「天」之间可能有空格（「5 天后」），扫描数字时要跳过空格，
///      但记录原文起点时要回到数字本身。
///   2. 数字解析失败时必须 `continue` 而不是 `?` 返回 ——
///      一个候选失败不代表整句没有相对日期表达。
fn scan_relative_days(s: &str) -> Option<(i64, String, i64)> {
    let chars: Vec<char> = s.chars().collect();

    for pos in 0..chars.len() {
        if chars[pos] != '天' {
            continue;
        }

        // 往前跳过空格，再扫数字
        let mut num_end = pos;
        while num_end > 0 && chars[num_end - 1].is_whitespace() {
            num_end -= 1;
        }
        if num_end == 0 {
            continue;
        }

        let mut start = num_end;
        while start > 0 && (is_cn_numeral(chars[start - 1]) || chars[start - 1].is_ascii_digit()) {
            start -= 1;
        }
        if start == num_end {
            continue;
        }

        let num_str: String = chars[start..num_end].iter().collect();
        let n = match parse_number(&num_str) {
            Some(v) if v > 0 => v,
            _ => continue,
        };

        // 后缀：后 / 前 / 之后 / 之前 / 以后 / 以前
        let after: String = chars[pos + 1..].iter().collect();
        let (sign, suffix_len) = if after.starts_with("之后") || after.starts_with("以后") {
            (1i64, 2usize)
        } else if after.starts_with("之前") || after.starts_with("以前") {
            (-1, 2)
        } else if after.starts_with('后') {
            (1, 1)
        } else if after.starts_with('前') {
            (-1, 1)
        } else {
            // 「3天」但没有后/前 → 不作为相对日期处理，避免误判
            continue;
        };

        // 原文范围覆盖数字到后缀，中间的空白也一并包含，便于整体剥离
        let matched: String = chars[start..(pos + 1 + suffix_len).min(chars.len())]
            .iter()
            .collect();
        return Some((n, matched, sign));
    }

    None
}

/// 解析绝对日期：`9月30日` `9月30号` `2026年9月30日` `2026-09-30` `十五号`。
fn parse_absolute_date(s: &str, today: NaiveDate) -> Option<(NaiveDate, String)> {
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;

    // 可选的年份
    let mut year: Option<i32> = None;
    {
        let (y, y_len) = scan_number(&chars);
        if y_len > 0 {
            let after = slice_from_chars(s, y_len);
            if after.starts_with('年') {
                if (1900..=2999).contains(&y) {
                    year = Some(y as i32);
                    i += y_len + 1; // 吃掉「年」
                }
            } else if after.starts_with('-') || after.starts_with('/') {
                // `2026-09-30` 形式
                if (1900..=2999).contains(&y) {
                    let sep = chars[y_len];
                    let rest = slice_from_chars(s, y_len + 1);
                    let (m, m_len) = scan_number_str(&rest);
                    let rest2 = slice_from_chars(&rest, m_len);
                    if m_len > 0 && rest2.starts_with(sep) {
                        let rest3 = slice_from_chars(&rest2, 1);
                        let (d, d_len) = scan_number_str(&rest3);
                        if d_len > 0 && (1..=12).contains(&m) && (1..=31).contains(&d) {
                            let date = NaiveDate::from_ymd_opt(y as i32, m as u32, d as u32)?;
                            let matched: String =
                                chars[..y_len + 1 + m_len + 1 + d_len].iter().collect();
                            return Some((date, matched));
                        }
                    }
                }
            }
        }
    }

    // 月日
    let (month, m_len) = scan_number(&chars[i..]);
    if m_len == 0 || !(1..=12).contains(&month) {
        return None;
    }
    let after_month = slice_from_chars(s, i + m_len);
    if !after_month.starts_with('月') {
        return None;
    }
    let after_month = slice_from_chars(&after_month, 1);
    let (day, d_len) = scan_number_str(&after_month);
    if d_len == 0 || !(1..=31).contains(&day) {
        return None;
    }
    let after_day = slice_from_chars(&after_month, d_len);
    let unit_len = if after_day.starts_with('日') || after_day.starts_with('号') {
        1
    } else {
        return None;
    };

    let y = match year {
        Some(y) => y,
        None => {
            // 未写年份：若该日期今年已过，则落到明年。
            // 这是符合直觉的行为 —— 12 月时说「1月5日」显然指明年。
            let candidate = NaiveDate::from_ymd_opt(today.year(), month as u32, day as u32)?;
            if candidate < today {
                today.year() + 1
            } else {
                today.year()
            }
        }
    };

    let date = NaiveDate::from_ymd_opt(y, month as u32, day as u32)?;
    let end = i + m_len + 1 + d_len + unit_len;
    let matched: String = chars[..end.min(chars.len())].iter().collect();
    Some((date, matched))
}

/// 解析「只有日、没有月」的表达，如 `十五号`、`30日`。
///
/// 这类表达在口语里很常见（"这个月十五号"）。语义上指**本月的该日**；
/// 若该日已过，则指下个月 —— 与"未写年份的月日落到明年"遵循同一条直觉。
fn parse_bare_day(s: &str, today: NaiveDate) -> Option<(NaiveDate, String)> {
    let (day, d_len) = scan_number_str(s);
    if d_len == 0 || !(1..=31).contains(&day) {
        return None;
    }

    let rest = slice_from_chars(s, d_len);
    if !(rest.starts_with('日') || rest.starts_with('号')) {
        return None;
    }

    let matched = slice_chars(s, 0, d_len + 1);

    // 先试本月
    if let Some(candidate) = NaiveDate::from_ymd_opt(today.year(), today.month(), day as u32) {
        if candidate >= today {
            return Some((candidate, matched));
        }
    }

    // 本月该日已过（或本月没有这一天，如 2 月 30 日）→ 落到下个月
    let (ny, nm) = if today.month() == 12 {
        (today.year() + 1, 1)
    } else {
        (today.year(), today.month() + 1)
    };
    // 下个月也可能没有这一天（如 1 月 31 日说「31号」而 2 月只有 28 天），
    // 此时取该月最后一天，与重复规则的月末处理保持一致。
    let last_day = {
        let (fy, fm) = if nm == 12 { (ny + 1, 1) } else { (ny, nm + 1) };
        NaiveDate::from_ymd_opt(fy, fm, 1)?.pred_opt()?.day()
    };
    let d = day.min(last_day as i64) as u32;
    Some((NaiveDate::from_ymd_opt(ny, nm, d)?, matched))
}

/// 解析纯时长表达：`3小时后` `30分钟后` `10秒后` `2天前`。
/// 返回 (**秒数**, 原文)。
///
/// 返回秒而非分钟：用户会用「10 秒后提醒我」来验证提醒是否工作，
/// 若以分钟为单位，10 秒会被截断成 0，表现为"输入了时间却没有任何提醒"。
///
/// 需要对每个候选位置**穷尽尝试**，不能一遇到解析失败就返回 None。
/// 早期实现在第一个「时」处用 `?` 提前退出：`3小时后` 的第一个「时」前面
/// 是「小」而非数字，于是整个函数直接失败，第二个「时」根本没被检查。
fn parse_duration(s: &str) -> Option<(i64, String)> {
    let chars: Vec<char> = s.chars().collect();

    for pos in 0..chars.len() {
        let unit_char = chars[pos];
        // 每个单位换算成多少**秒**。
        //
        // 「小」也是候选位置（「3小时」里会被扫到），因此它的换算与「时」相同；
        // 后面统一由 unit_end 把「小时」「分钟」这类双字单位补全。
        let per_unit = match unit_char {
            '秒' => 1i64,
            '分' | '钟' => 60,
            '时' | '小' => 3600,
            '天' => 86400,
            _ => continue,
        };

        // 往前扫数字
        let mut start = pos;
        while start > 0 && (is_cn_numeral(chars[start - 1]) || chars[start - 1].is_ascii_digit()) {
            start -= 1;
        }
        if start == pos {
            continue;
        }

        // 数字与单位之间不能夹着另一个时间单位字。
        //
        // 这个校验是必需的，否则会截出错误的数字：`30秒钟后` 在扫描到「钟」时，
        // 它前面是「秒」而不是数字，于是数字扫描只抓到「3」，算出 3 秒 ——
        // 而用户写的是 30 秒，差了 10 倍。这类错误不会报错，
        // 只会让提醒在完全错误的时间触发，极难排查。
        if is_time_unit_char(chars[start]) {
            continue;
        }

        let num_str: String = chars[start..pos].iter().collect();
        // 注意这里不能用 `?`：一个数字片段解析失败不代表整句没有时长表达
        let n = match parse_number(&num_str) {
            Some(v) if v > 0 => v,
            _ => continue,
        };

        // 数字前可能还有「小」，它属于表达的一部分（「3小时」）
        let real_start = if start > 0 && chars[start - 1] == '小' {
            start - 1
        } else {
            start
        };

        // 单位字符后面可能还有第二个字，它们同属表达的一部分：
        //   「小」+「时」→ 「3小时」
        //   「分」+「钟」→ 「30分钟」
        //   「秒」+「钟」→ 「30秒钟」
        // 漏掉任何一个都会让对应写法失配。
        let mut unit_end = pos + 1;
        if chars[pos] == '小' && chars.get(unit_end) == Some(&'时') {
            unit_end += 1;
        }
        if (chars[pos] == '分' || chars[pos] == '秒') && chars.get(unit_end) == Some(&'钟') {
            unit_end += 1;
        }

        // 后缀：后 / 前 / 之后 / 之前 / 以后 / 以前
        let after: String = chars[unit_end.min(chars.len())..].iter().collect();
        let (sign, suffix_len) = if after.starts_with("之后") || after.starts_with("以后") {
            (1i64, 2usize)
        } else if after.starts_with("之前") || after.starts_with("以前") {
            (-1, 2)
        } else if after.starts_with('后') {
            (1, 1)
        } else if after.starts_with('前') {
            (-1, 1)
        } else {
            // 「3小时」但后面既无「后」也无「前」→ 不是时长表达
            continue;
        };

        let end = unit_end + suffix_len;
        let matched: String = chars[real_start.min(chars.len())..end.min(chars.len())]
            .iter()
            .collect();

        // **必须夹住，且要在乘法之前先把 `n` 收敛。**
        //
        // `n` 来自用户输入且完全无界，而 `per_unit` 最大是 86400（天），
        // 因此 `n * per_unit` 本身就可能算术溢出（debug 下直接 panic）；
        // 即便不溢出，也会得到一个让 chrono 日期加法 panic 的值 ——
        // release 下 `panic = "abort"`，整个进程连同托盘与所有提醒一起消失，
        // 而且用户只是在输入框里打字（预览会在停顿 180ms 后触发）。
        let max_units = crate::domain::time::MAX_DURATION_SECONDS / per_unit.max(1);
        let n = n.min(max_units);
        let offset = n.saturating_mul(per_unit).saturating_mul(sign);

        return Some((crate::domain::time::clamp_duration_seconds(offset), matched));
    }

    None
}

/// 按「本周 / 下周」的语义计算目标日期。
///
/// 先定位到**本周一**，再叠加周偏移与目标星期几的位移。
///
/// 两处关键：
///   1. 不能用"下一个该星期几"作基准再加周偏移 —— 那会让周二说的「下周一」
///      变成 14 天后这种荒谬结果。"下周的周一"应以自然周为基准。
///   2. "已过则顺延一周"只在 `week_offset == 0` 时适用。
///      对「下周一」而言，即便本周一已过，目标也仍是下周的周一，
///      不该再额外顺延到再下一周。
fn next_weekday(today: NaiveDate, target: Weekday, week_offset: i64) -> NaiveDate {
    // 本周一
    let monday = today - Duration::days(today.weekday().num_days_from_monday() as i64);
    // 目标星期几相对本周一的位移
    let shift = target.num_days_from_monday() as i64;
    let mut candidate = monday + Duration::days(shift + week_offset * 7);

    // 只在"本周内"的语义下才做顺延，保证结果总是未来。
    // 周二说「周一」指下周一；而说「下周一」时 candidate 已经是未来，不会触发。
    if week_offset == 0 && candidate <= today {
        candidate += Duration::days(7);
    }

    candidate
}

/// 匹配星期几字符，返回 (星期, 消耗字符数)。
fn match_weekday_char(s: &str) -> (Option<Weekday>, usize) {
    for (pat, day) in [
        ("周一", Weekday::Mon),
        ("星期一", Weekday::Mon),
        ("礼拜一", Weekday::Mon),
        ("周二", Weekday::Tue),
        ("星期二", Weekday::Tue),
        ("礼拜二", Weekday::Tue),
        ("周三", Weekday::Wed),
        ("星期三", Weekday::Wed),
        ("礼拜三", Weekday::Wed),
        ("周四", Weekday::Thu),
        ("星期四", Weekday::Thu),
        ("礼拜四", Weekday::Thu),
        ("周五", Weekday::Fri),
        ("星期五", Weekday::Fri),
        ("礼拜五", Weekday::Fri),
        ("周六", Weekday::Sat),
        ("星期六", Weekday::Sat),
        ("礼拜六", Weekday::Sat),
        ("周日", Weekday::Sun),
        ("周天", Weekday::Sun),
        ("星期日", Weekday::Sun),
        ("星期天", Weekday::Sun),
        ("礼拜日", Weekday::Sun),
        ("礼拜天", Weekday::Sun),
    ] {
        if s.starts_with(pat) {
            return (Some(day), pat.chars().count());
        }
    }
    (None, 0)
}

/// 匹配**裸**星期字符（「一」「三」「日」），用于已经剥掉「下周」「本周」前缀之后。
///
/// 单独一个函数而不复用 [`match_weekday_char`]：带前缀的匹配要求出现「周/星期/礼拜」，
/// 而剥掉前缀后剩下的正是那个裸字符，用前者永远匹配不到。
fn match_bare_weekday(s: &str) -> Option<Weekday> {
    match s.chars().next()? {
        '一' => Some(Weekday::Mon),
        '二' => Some(Weekday::Tue),
        '三' => Some(Weekday::Wed),
        '四' => Some(Weekday::Thu),
        '五' => Some(Weekday::Fri),
        '六' => Some(Weekday::Sat),
        '日' | '天' => Some(Weekday::Sun),
        _ => None,
    }
}

/// 星期几对应的单字，用于拼接命中原文。
fn weekday_char(w: Weekday) -> char {
    match w {
        Weekday::Mon => '一',
        Weekday::Tue => '二',
        Weekday::Wed => '三',
        Weekday::Thu => '四',
        Weekday::Fri => '五',
        Weekday::Sat => '六',
        Weekday::Sun => '日',
    }
}

// ============================================================================
// 数字扫描
// ============================================================================

/// 从字符串开头扫描一个数字。是 [`scan_number`] 的便捷包装。
///
/// 存在的理由：调用方几乎总是拿着 `String` 而不是 `&[char]`，
/// 若每处都写 `.chars().collect::<Vec<_>>()` 会让逻辑被噪音淹没。
fn scan_number_str(s: &str) -> (i64, usize) {
    let chars: Vec<char> = s.chars().collect();
    scan_number(&chars)
}

/// 从字符序列开头扫描一个数字，返回 (数值, 消耗的字符数)。
/// 支持阿拉伯数字与中文数字。
fn scan_number(chars: &[char]) -> (i64, usize) {
    if chars.is_empty() {
        return (0, 0);
    }

    // 阿拉伯数字
    if chars[0].is_ascii_digit() {
        let mut n: i64 = 0;
        let mut len = 0;
        while len < chars.len() && chars[len].is_ascii_digit() {
            n = n * 10 + (chars[len] as u8 - b'0') as i64;
            len += 1;
            if n > 100_000 {
                return (0, 0);
            }
        }
        return (n, len);
    }

    // 中文数字
    let mut len = 0;
    while len < chars.len() && is_cn_numeral(chars[len]) {
        len += 1;
    }
    if len == 0 {
        return (0, 0);
    }

    let text: String = chars[..len].iter().collect();
    match parse_number(&text) {
        Some(n) => (n, len),
        None => (0, 0),
    }
}

fn is_cn_numeral(c: char) -> bool {
    matches!(
        c,
        '一' | '二' | '两' | '三' | '四' | '五' | '六' | '七' | '八' | '九' | '十' | '零' | '〇'
    )
}

/// 是否是时间单位字（秒 / 分 / 钟 / 时 / 小 / 天）。
///
/// 用于判断"数字与单位之间是否夹着另一个单位字" —— 那说明当前位置其实属于
/// 一个更长的单位（如「分钟」里的「钟」），不该被当成独立单位处理。
/// 没有这道校验的话，「30秒钟后」会被算成 3 秒（数字扫描停在「秒」前）。
fn is_time_unit_char(c: char) -> bool {
    matches!(c, '秒' | '分' | '钟' | '时' | '小' | '天')
}

/// 解析数字字符串，支持阿拉伯数字与中文数字（一 至 九十九）。
fn parse_number(s: &str) -> Option<i64> {
    if s.is_empty() {
        return None;
    }

    if s.chars().all(|c| c.is_ascii_digit()) {
        return s.parse().ok();
    }

    let chars: Vec<char> = s.chars().collect();
    let digit = |c: char| -> Option<i64> {
        match c {
            '零' | '〇' => Some(0),
            '一' => Some(1),
            '二' | '两' => Some(2),
            '三' => Some(3),
            '四' => Some(4),
            '五' => Some(5),
            '六' => Some(6),
            '七' => Some(7),
            '八' => Some(8),
            '九' => Some(9),
            _ => None,
        }
    };

    if chars.len() == 1 {
        if chars[0] == '十' {
            return Some(10);
        }
        return digit(chars[0]);
    }

    if let Some(pos) = chars.iter().position(|c| *c == '十') {
        let tens = if pos == 0 { 1 } else { digit(chars[pos - 1])? };
        let ones = if pos + 1 < chars.len() {
            digit(chars[pos + 1])?
        } else {
            0
        };
        // 只支持两位数的中文数字。超过两位数（如「一百」）不解析，
        // 因为待办场景里几乎不会出现，而支持它会让扫描逻辑复杂化。
        if pos > 1 || chars.len() > pos + 2 {
            return None;
        }
        return Some(tens * 10 + ones);
    }

    // 纯数字串（如「三〇」不常见，此处仅处理单字符已覆盖的情况）
    if chars.len() == 1 {
        return digit(chars[0]);
    }

    None
}

// ============================================================================
// 字符串工具
// ============================================================================

/// 取字符串前 `n` 个字符。
///
/// 用字符而非字节，避免在多字节中文上切出不合法边界而 panic。
fn slice_chars(s: &str, start: usize, end: usize) -> String {
    s.chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect()
}

/// 从第 `n` 个字符开始取到结尾。
fn slice_from_chars(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

/// 移除 `s` 中第一次出现的 `pat`，并规整空白。
///
/// **剥离后必须 trim**：各解析函数都假设时间表达位于字符串开头，
/// 若剥离后留下前导空格，后续的"从开头扫描"会直接失败。
/// 例如 `明天下午3点 交房租` 剥掉日期后为 `下午3点 交房租`，
/// 不去掉空格就再也解析不出时刻。
///
/// `replacen` 而非 `replace`：时间表达在标题里可能有重复词
/// （如「明天 明天开会」），只应移除被解析掉的那一次。
fn strip_first(s: &str, pat: &str) -> String {
    if pat.is_empty() {
        return s.to_string();
    }
    collapse_spaces(s.replacen(pat, "", 1).trim())
}

/// 把连续空白压成单个空格并去掉首尾空白。
///
/// 压制连续空白是必要的：剥离会留下"空洞"，`明天 下午3点 开会`
/// 去掉两段时间后会剩下多个连续空格。
fn collapse_spaces(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            if !prev_space {
                out.push(' ');
                prev_space = true;
            }
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    out.trim().to_string()
}

/// 如果 `s` 以 `pat` 开头，返回去掉前缀后的剩余部分（已规整空白）。
fn strip_prefix_any(s: &str, pats: &[&str]) -> Option<String> {
    for p in pats {
        if let Some(rest) = s.strip_prefix(p) {
            // 必须 trim：调用方紧接着会做"从开头扫描"
            return Some(collapse_spaces(rest));
        }
    }
    None
}

/// 移除开头的前缀（若存在），并规整空白。
///
/// 同样要 trim：调用方紧接着会做"从开头扫描"，前导空格会导致失败。
fn strip_first_once(s: &str, pat: &str) -> String {
    collapse_spaces(s.strip_prefix(pat).unwrap_or(s))
}

/// 返回 `s` 开头的第一个匹配模式的**字符数**（无匹配则 0）。
///
/// 刻意只返回长度而不返回匹配到的字符串：返回 `&'static str` 会迫使调用方
/// 处理生命周期，而早期版本为此用了 `Box::leak` —— 那是纯粹的内存泄漏，
/// 只为了迁就一个并不需要的 API 形状。调用方需要原文时用 `slice_chars` 取即可。
fn match_any_len(s: &str, pats: &[&str]) -> usize {
    for p in pats {
        if s.starts_with(p) {
            return p.chars().count();
        }
    }
    0
}

/// 清理标题：去掉剥离时间表达后残留的多余空白与标点。
fn clean_title(s: &str) -> String {
    let mut out = s.trim().to_string();

    // 去掉因剥离产生的连续空白
    while out.contains("  ") {
        out = out.replace("  ", " ");
    }

    // 去掉开头残留的分隔标点
    out = out
        .trim_start_matches([',', '，', '、', '。', '.', ':', '：', ';', '；'])
        .trim()
        .to_string();

    out
}

#[cfg(test)]
mod tests;
