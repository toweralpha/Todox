//! 暴露给前端的 Tauri 命令。
//!
//! 本层刻意保持极薄：只做参数转换与错误转成可序列化的字符串，
//! 不包含任何业务逻辑。所有校验与副作用都在 repository 层，
//! 这样测试与未来的其他入口都能复用同一套规则。

// `Datelike` 提供 NaiveDate::weekday()。trait 方法必须显式导入才能调用，
// 否则会报 "method weekday is private" 这种容易误导的错误。
use chrono::{Datelike, NaiveDateTime};
use serde::Serialize;
use tauri::State;

use crate::db::connection::Db;
use crate::domain::recurrence::{describe, RecurrenceRule};
use crate::domain::task::{NewTask, Task, TaskCompletion, TaskEdit, TimeKind};
use crate::repo::export_repo::{ExportEnvelope, ImportSummary};
use crate::repo::recurrence_repo::RecurrenceRepo;
use crate::repo::reminder_repo::{MissedReminderRow, ReminderRepo};
use crate::repo::settings_repo::{AppSettings, SettingsRepo};
use crate::repo::stats_repo::{DailyCount, StatsOverview, StatsRepo, TaskStreak};
use crate::repo::task_repo::{RepoError, TaskRepo};
use crate::AppRuntime;

/// 统一把仓储错误转成前端可读的中文消息。
///
/// Tauri 要求命令的错误类型可序列化，`RepoError` 内部持有 `rusqlite::Error`
/// 无法直接序列化。这里转成 `String` 是刻意的取舍：错误在跨进程边界后
/// 主要用于展示与日志，丢失类型结构的影响很小。
fn to_msg(e: RepoError) -> String {
    e.to_string()
}

/// 通知调度器"提醒集合可能变了，重算下次触发时刻"。
///
/// **任何改动任务的命令都必须调用它。** 漏调的后果是提醒延迟生效：
/// 用户新建一个"5 分钟后"的任务，而调度器可能正睡在 3 小时后，
/// 那条提醒要等到调度器自然醒来才弹 —— 对定时提醒来说这等于失效。
///
/// 类型是 `State<AppRuntime>` 而不是 `Option<State<..>>`：Tauri 会把
/// 非注入类型当作需要反序列化的命令行参数，`Option<State<..>>` 会因此
/// 编译失败。需要绕开 Tauri 运行时的测试应调用本模块里的 `*_with`
/// 自由函数，而不是试图给命令传 `None`。
fn wake(runtime: State<'_, AppRuntime>) {
    runtime.scheduler.wake();
}

#[tauri::command]
pub fn create_task(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    input: NewTask,
) -> Result<Task, String> {
    let task = TaskRepo::new(&db).create(input).map_err(to_msg)?;
    // 新任务需要按设置落库默认提醒档位，否则它永远不会被调度
    ReminderRepo::new(&db)
        .create_defaults(&task)
        .map_err(to_msg)?;
    wake(runtime);
    Ok(task)
}

#[tauri::command]
pub fn list_tasks(db: State<'_, Db>, include_completed: bool) -> Result<Vec<Task>, String> {
    TaskRepo::new(&db)
        .list_all(include_completed)
        .map_err(to_msg)
}

#[tauri::command]
pub fn delete_task(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    id: String,
) -> Result<(), String> {
    TaskRepo::new(&db).soft_delete(&id).map_err(to_msg)?;
    // 删除任务后它不应再触发提醒，必须唤醒调度器重算
    wake(runtime);
    Ok(())
}

#[tauri::command]
pub fn restore_task(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    id: String,
) -> Result<Task, String> {
    let t = TaskRepo::new(&db).restore(&id).map_err(to_msg)?;
    wake(runtime);
    Ok(t)
}

#[tauri::command]
pub fn complete_task(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    id: String,
) -> Result<(), String> {
    TaskRepo::new(&db).complete(&id).map_err(to_msg)?;
    // 完成会推进重复任务的下一次发生时间，提醒集合因此改变
    wake(runtime);
    Ok(())
}

#[tauri::command]
pub fn uncomplete_task(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    id: String,
) -> Result<(), String> {
    TaskRepo::new(&db).uncomplete(&id).map_err(to_msg)?;
    wake(runtime);
    Ok(())
}

#[tauri::command]
pub fn task_completions(db: State<'_, Db>, id: String) -> Result<Vec<TaskCompletion>, String> {
    TaskRepo::new(&db).completions_of(&id).map_err(to_msg)
}

#[tauri::command]
pub fn unfinished_count(db: State<'_, Db>) -> Result<i64, String> {
    TaskRepo::new(&db).count_unfinished().map_err(to_msg)
}

/// 自检命令：返回各表行数。
///
/// 保留它不是为了调试方便，而是为了让"数据库确实在工作"这件事可被外部验证 ——
/// 阶段六的性能与稳定性验证需要一条不依赖界面的观测途径。
#[tauri::command]
pub fn table_counts(db: State<'_, Db>) -> Result<Vec<(String, i64)>, String> {
    TaskRepo::new(&db).table_counts().map_err(to_msg)
}

/// 解析结果的展示形态。
///
/// 与 `nlp::ParsedTime` 分开而不是直接返回它：前端需要的是"可直接显示的中文
/// 标签"，而不是裸时间戳。让解析结果与展示格式各司其职，避免前端重复实现
/// 一套格式化逻辑（两边实现必然会产生不一致）。
#[derive(Debug, Serialize)]
pub struct ParsePreview {
    /// 去掉时间表达后的标题
    pub title: String,
    /// 界面上显示的时间标签，例如「明天 15:00」「每周一 09:00」
    pub time_label: Option<String>,
    /// 重复规则的中文描述，例如「每周一 09:00」
    pub recurrence_label: Option<String>,
    /// 时间类型
    pub time_kind: TimeKind,
    /// 能否解析出时间。为 false 时界面应明确告知用户"未识别时间"，
    /// 而不是让用户以为自己写的被理解了。
    pub has_time: bool,
    /// 命中的时间表达原文，便于用户确认解析是否正确
    pub matched_text: Option<String>,
}

/// 仅解析、不落库。用于输入框的实时预览。
///
/// 之所以把解析放在 Rust 侧而不是前端：解析规则与 `domain::recurrence` 的
/// 语义紧密相关，分开实现迟早会出现"预览显示一套、实际存储另一套"的问题。
#[tauri::command]
pub fn parse_input(text: String) -> ParsePreview {
    let now = chrono::Local::now().naive_local();
    build_preview(&text, now)
}

/// 解析并创建任务。
///
/// 这是快速添加框的主入口：把"解析 → 必要时建重复规则 → 建任务"放在一个
/// 命令里，避免前端分两次调用导致中途失败时留下孤儿规则。
#[tauri::command]
pub fn create_task_from_text(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    text: String,
) -> Result<Task, String> {
    let now = chrono::Local::now().naive_local();
    let task = create_from_text_with(&db, &text, now)?;
    wake(runtime);
    Ok(task)
}

/// [`create_task_from_text`] 的实现，`now` 由调用方注入。
///
/// 与命令分开是为了**可测试**：命令层依赖 Tauri 运行时（`State`、`AppHandle`），
/// 而集成测试需要用固定的 `now` 才能得到确定性结果 —— 依赖系统当前时间的
/// 测试会随真实日期漂移，今天通过明天失败。
pub fn create_from_text_with(db: &Db, text: &str, now: NaiveDateTime) -> Result<Task, String> {
    let parsed = crate::nlp::parse(text, now);

    let title = if parsed.title.trim().is_empty() {
        // 用户只输入了时间表达（如「明天下午3点」）。
        // 此时不能拒绝创建，否则会丢掉用户已经写下的内容；
        // 用时间表达原文作为标题，用户可在之后编辑。
        parsed
            .matched_text
            .clone()
            .unwrap_or_else(|| text.trim().to_string())
    } else {
        parsed.title.trim().to_string()
    };

    if title.is_empty() {
        return Err("请输入任务内容".into());
    }

    let repo = TaskRepo::new(db);

    // 先建重复规则（若解析出了重复语义）
    let mut recurrence_id = None;
    if let Some(mut rule) = parsed.recurrence.clone() {
        // 补上"每次发生的时间点"，否则引擎只能沿用上一次的时刻，
        // 对于"每周一早上9点"这类表达就会丢失 09:00 这个信息。
        if rule.at_time_of_day.is_none() {
            if let Some(at) = parsed.at {
                rule.at_time_of_day = Some(at.format("%H:%M").to_string());
            }
        }
        let id = RecurrenceRepo::new(db).create(&rule).map_err(to_msg)?;
        recurrence_id = Some(id);
    }

    // 决定时间类型与各时间字段
    let (time_kind, due_at, deadline_at) = if let Some(rule) = parsed.recurrence.as_ref() {
        // 重复任务的首次发生时间由**规则本身**推导，
        // 不套用 parse 的"今天已过则顺延到明天"逻辑（那与星期几无关，会算错）。
        let tod = rule
            .at_time_of_day
            .as_deref()
            .and_then(crate::domain::recurrence::parse_time_of_day)
            .or_else(|| parsed.at.map(|a| a.time()))
            // 规则未指定时刻时退化为当天开始，避免因缺少时刻而无法创建
            .unwrap_or_else(|| chrono::NaiveTime::from_hms_opt(0, 0, 0).expect("0 点必然合法"));

        let due = first_occurrence_from_rule(rule, tod, now)?;
        (TimeKind::Recurring, Some(due), None)
    } else if let Some(at) = parsed.at {
        (TimeKind::AtTime, Some(at.to_string()), None)
    } else if let Some(offset) = parsed.offset_seconds {
        // 相对表达（「3小时后」「10秒后」）换算成绝对时刻。
        //
        // 两处必须同时成立：
        //   1. 单位是秒 —— 用 minutes() 会把 10 秒截断成 0，
        //      表现就是"输入了时间但任务落在收件箱里"。
        //   2. 必须夹住上限 —— chrono 的日期加法在越界时**会 panic**，
        //      而 release 下 `panic = "abort"` 会直接杀掉整个进程。
        //      解析层已经收敛过一次，这里是第二道防线（防御性重复，
        //      因为它同时也是"任何其它来源的 offset"的必经之路）。
        let offset = crate::domain::time::clamp_duration_seconds(offset);
        let due = now + chrono::Duration::seconds(offset);
        (TimeKind::AtTime, Some(due.to_string()), None)
    } else if let Some(d) = parsed.date {
        // 只有日期没有时刻 → 全天任务
        (TimeKind::AllDay, Some(d.to_string()), None)
    } else {
        // 完全没解析出时间 → 收件箱
        (TimeKind::AllDay, None, None)
    };

    // to_string() 产出的是无时区格式，需要补上本地偏移，
    // 否则存进数据库的时间无法与其它记录比较先后。
    let due_at = due_at.map(|s| attach_local_offset(&s));

    let input = NewTask {
        title,
        note: None,
        time_kind,
        due_at,
        deadline_at,
        recurrence_id,
        priority: crate::domain::task::Priority::None,
    };

    let task = repo.create(input).map_err(to_msg)?;

    // 按设置里的默认策略为任务落库提醒档位。
    // 不做这一步的任务永远不会被调度器看见 —— 它只是静静地躺在列表里。
    // 失败时不回滚任务：任务本身已经正确保存，缺提醒比丢任务好得多。
    if let Err(e) = ReminderRepo::new(db).create_defaults(&task) {
        eprintln!("为任务 {} 创建默认提醒失败：{e}", task.id);
    }

    Ok(task)
}

/// 从重复规则推导**首次发生时间**。
///
/// 为什么不能复用 `parse` 的结果：`parse_recurrence` 会把「每周一」整段消耗掉，
/// 因此 `parse` 返回的 `date` 恒为 `None`；而 `at` 对"今天已过的时刻"会顺延到
/// 明天，得到的是"明天 09:00"这种与星期几无关的时刻。
///
/// 对重复任务而言，首次发生必须**由规则本身决定**：从今天起逐日寻找第一个
/// 满足规则星期掩码、且该时刻尚未过去的日子。这样「每周一早上9点」在周二
/// 得到的就是下周一，而不是周三。
///
/// 与 `domain::recurrence::next_occurrence` 的分工：后者负责**后续**推进，
/// 本函数只负责**首次**落地。
fn first_occurrence_from_rule(
    rule: &RecurrenceRule,
    time_of_day: chrono::NaiveTime,
    now: NaiveDateTime,
) -> Result<String, String> {
    let mut date = now.date();

    // 最多看 8 天，足以覆盖"每周某天"（最长间隔 7 天）。
    for _ in 0..8 {
        let weekday_ok = match rule.by_weekdays {
            Some(mask) if mask != 0 => {
                crate::domain::recurrence::mask_contains(mask, date.weekday())
            }
            // 未指定星期几（每天 / 每 N 天）时任何一天都满足
            _ => true,
        };

        // 只有"全天允许 + 该时刻尚未过去"才落在这天。
        // 时刻已过就顺延一天 —— 但仅在星期掩码也允许时才会用到顺延后的日子，
        // 因此顺延不会破坏"每周一"的星期约束。
        if weekday_ok {
            let candidate = date.and_time(time_of_day);
            if candidate > now {
                return Ok(candidate.to_string());
            }
        }

        date += chrono::Duration::days(1);
    }

    Err("无法为重复任务确定首次发生时间".into())
}

/// 给不带时区的时间串补上本地时区偏移。
///
/// **必须容错两种格式**：chrono 的 `NaiveDateTime::to_string()` 产出的是
/// `"2026-09-30 15:00:00"`（空格分隔、无偏移），而手写的 ISO 串可能是
/// `"2026-09-30T15:00:00"`（T 分隔）。
///
/// 早期实现只认后者，于是前者被静默原样透传，数据库里就混入了不带偏移的
/// 时间戳。这会破坏"所有时间戳都可比较先后"这一不变量，而且极难察觉 ——
/// 写入看起来完全正常，只有排序和倒计时会出错。
fn attach_local_offset(s: &str) -> String {
    use chrono::TimeZone;

    // 已经带了偏移，原样返回
    if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
        return s.to_string();
    }

    // 依次尝试：两种带时间的分隔符，以及**纯日期**。
    //
    // 纯日期这一条不能漏。`NaiveDate::to_string()` 产出的是 `"2026-09-30"`
    // （10 字节、没有时间部分），而它正是自然语言里「明天 买牛奶」这类
    // 全天任务落库时的形态。chrono 的 RFC3339 解析要求至少 19 字节，
    // 因此漏掉这一条会让该字符串被**原样透传**，调度器随后解析失败并跳过任务 ——
    // 表现为"任务带日期地躺在列表里，但永远不会响"。
    //
    // 补成当天 00:00 而不是当前时刻：全天任务的语义是"那一天"，
    // 具体提醒钟点由调度器的 all_day_base 统一规整到用户设定值（默认 09:00）。
    let naive = [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ]
    .iter()
    .find_map(|fmt| NaiveDateTime::parse_from_str(s, fmt).ok())
    .or_else(|| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    });

    // 解析不了就原样返回，交由数据层校验报错。
    // 不在这里臆造一个时间 —— 静默改写用户数据比报错更糟。
    let naive = match naive {
        Some(n) => n,
        None => return s.to_string(),
    };

    match chrono::Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.to_rfc3339(),
        // 夏令时重复的时刻取较早一次，保证行为确定
        chrono::LocalResult::Ambiguous(earlier, _) => earlier.to_rfc3339(),
        // 夏令时跳过的时刻顺延一小时，保证总能得到有效时间。
        // 这里也必须带上偏移 —— 早期版本直接 to_string()，
        // 结果又产出不带偏移的时间戳，等于把刚修好的问题重新引入。
        chrono::LocalResult::None => {
            let shifted = naive + chrono::Duration::hours(1);
            match chrono::Local.from_local_datetime(&shifted) {
                chrono::LocalResult::Single(dt) => dt.to_rfc3339(),
                chrono::LocalResult::Ambiguous(earlier, _) => earlier.to_rfc3339(),
                chrono::LocalResult::None => shifted.format("%Y-%m-%dT%H:%M:%S").to_string(),
            }
        }
    }
}

/// 构造解析预览。
fn build_preview(text: &str, now: NaiveDateTime) -> ParsePreview {
    let parsed = crate::nlp::parse(text, now);

    let has_time = parsed.at.is_some() || parsed.date.is_some() || parsed.offset_seconds.is_some();

    let recurrence_label = parsed
        .recurrence
        .as_ref()
        .map(|r: &RecurrenceRule| describe(r));

    // 时间标签：优先显示具体时刻，其次是日期
    let time_label = if let Some(at) = parsed.at {
        Some(format!(
            "{} {}",
            crate::domain::time::human_date(at.date(), now.date()),
            at.format("%H:%M")
        ))
    } else if let Some(d) = parsed.date {
        Some(crate::domain::time::human_date(d, now.date()))
    } else if let Some(offset) = parsed.offset_seconds {
        // 预览里同样按秒换算并夹住上限，保证"预览显示的时间"与"实际存的时间"一致。
        // 这条路径尤其重要：它在用户**打字停顿 180ms 后**就会被调用，
        // 因此一个越界值会在用户还没点保存时就把进程带走。
        let offset = crate::domain::time::clamp_duration_seconds(offset);
        let target = now + chrono::Duration::seconds(offset);
        Some(format!(
            "{} {}",
            crate::domain::time::human_date(target.date(), now.date()),
            target.format("%H:%M")
        ))
    } else {
        None
    };

    // 有重复规则 → 重复任务；有时刻或时长 → 时间点；其余（含只有日期）→ 全天。
    let time_kind = if parsed.recurrence.is_some() {
        TimeKind::Recurring
    } else if parsed.at.is_some() || parsed.offset_seconds.is_some() {
        TimeKind::AtTime
    } else {
        TimeKind::AllDay
    };

    ParsePreview {
        title: parsed.title,
        time_label,
        recurrence_label,
        time_kind,
        has_time,
        matched_text: parsed.matched_text,
    }
}

// ============================================================================
// 设置
// ============================================================================

#[tauri::command]
pub fn get_settings(db: State<'_, Db>) -> Result<AppSettings, String> {
    SettingsRepo::new(&db).load().map_err(to_msg)
}

/// 保存设置。
///
/// 保存后必须唤醒调度器：提醒档位、勿扰时段、全天提醒钟点都直接影响
/// "什么时候该弹"，改动后不重算就等于没生效。
#[tauri::command]
pub fn save_settings(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    settings: AppSettings,
) -> Result<(), String> {
    SettingsRepo::new(&db).save(&settings).map_err(to_msg)?;
    wake(runtime);
    Ok(())
}

// ============================================================================
// 错过的提醒
// ============================================================================

/// 未确认的错过记录。
///
/// 前端据此显示"你错过了 X 个提醒"。刻意不在这里自动确认：
/// 用户需要先看到它们，而不是被程序替他决定"已经知道了"。
#[tauri::command]
pub fn pending_missed(db: State<'_, Db>) -> Result<Vec<MissedReminderRow>, String> {
    ReminderRepo::new(&db).pending_missed().map_err(to_msg)
}

#[tauri::command]
pub fn acknowledge_missed(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    id: String,
) -> Result<(), String> {
    ReminderRepo::new(&db)
        .acknowledge_missed(&id)
        .map_err(to_msg)?;
    wake(runtime);
    Ok(())
}

#[tauri::command]
pub fn acknowledge_all_missed(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
) -> Result<usize, String> {
    let n = ReminderRepo::new(&db)
        .acknowledge_all_missed()
        .map_err(to_msg)?;
    wake(runtime);
    Ok(n)
}

// ============================================================================
// 稍后提醒
// ============================================================================

/// 把某个提醒档位推迟若干分钟。
///
/// `snooze_until` 一旦写入就会取代档位偏移作为触发时刻（见调度器的
/// `upcoming_reminders`），因此调度器只需正常重算，不需要特殊分支。
///
/// 触发后调度器会清除这个字段（见调度器的 `fire`）—— 不清除的话，
/// 那个时刻会永远停在过去，变成无限重复提醒。
#[tauri::command]
pub fn snooze_reminder(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    task_id: String,
    offset_seconds: i64,
    minutes: i64,
) -> Result<(), String> {
    // 夹到合理区间，避免界面传入负数或超长时长把提醒推到不可预期的时刻
    let minutes = minutes.clamp(1, 24 * 60);
    let until = (chrono::Local::now() + chrono::Duration::minutes(minutes)).to_rfc3339();

    ReminderRepo::new(&db)
        .snooze(&task_id, offset_seconds, &until)
        .map_err(to_msg)?;
    wake(runtime);
    Ok(())
}

/// 取消某档位的稍后提醒，让它回到原定时刻。
#[tauri::command]
pub fn clear_snooze(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    task_id: String,
    offset_seconds: i64,
) -> Result<(), String> {
    ReminderRepo::new(&db)
        .clear_snooze(&task_id, offset_seconds)
        .map_err(to_msg)?;
    wake(runtime);
    Ok(())
}

// ============================================================================
// 编辑任务
// ============================================================================

/// 修改任务的部分字段。
///
/// 这是"截止型任务"的创建入口的前提 —— 自然语言解析器目前不产生
/// `before_deadline`，用户需要能通过界面把任务改成截止型。
#[tauri::command]
pub fn update_task(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    id: String,
    edit: TaskEdit,
) -> Result<Task, String> {
    let task = TaskRepo::new(&db).update(&id, &edit).map_err(to_msg)?;

    // 时间类型可能变了（例如从「时间点」改成「截止型」），
    // 那意味着提醒档位也该跟着变（一个是到点提醒，一个是三级分级提醒）。
    // create_defaults 只在无档位时插入，因此需要先清掉旧的。
    if edit.time_kind.is_some() {
        if let Err(e) = ReminderRepo::new(&db).reset_defaults(&task) {
            eprintln!("重置任务 {} 的提醒档位失败：{e}", task.id);
        }
    }

    wake(runtime);
    Ok(task)
}

// ============================================================================
// 完成统计
// ============================================================================

#[tauri::command]
pub fn stats_overview(db: State<'_, Db>) -> Result<StatsOverview, String> {
    StatsRepo::new(&db).overview().map_err(to_msg)
}

#[tauri::command]
pub fn stats_daily(db: State<'_, Db>, days: i64) -> Result<Vec<DailyCount>, String> {
    StatsRepo::new(&db).daily_counts(days).map_err(to_msg)
}

#[tauri::command]
pub fn stats_by_task(db: State<'_, Db>, limit: i64) -> Result<Vec<TaskStreak>, String> {
    StatsRepo::new(&db).by_task(limit).map_err(to_msg)
}

// ============================================================================
// 导入导出
// ============================================================================

/// 导出全部数据。
///
/// 命令只负责把数据交回前端，**文件写在哪里由前端通过系统对话框决定**。
/// 这样后端不需要处理路径合法性，也不会被诱导去写任意位置。
#[tauri::command]
pub fn export_data(db: State<'_, Db>) -> Result<ExportEnvelope, String> {
    crate::repo::export_repo::export(&db).map_err(to_msg)
}

/// 导入数据。
///
/// `replace_existing` 为 true 是"恢复"（清空后写入），
/// 为 false 是"合并"（按主键覆盖同 ID 的行）。
#[tauri::command]
pub fn import_data(
    db: State<'_, Db>,
    runtime: State<'_, AppRuntime>,
    envelope: ExportEnvelope,
    replace_existing: bool,
) -> Result<ImportSummary, String> {
    let summary =
        crate::repo::export_repo::import(&db, &envelope, replace_existing).map_err(to_msg)?;
    // 导入可能带来成百上千条提醒，必须让调度器重算
    wake(runtime);
    Ok(summary)
}
