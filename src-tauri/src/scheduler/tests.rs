//! 调度器的单元测试。
//!
//! 这些测试刻意**不启动真实的异步循环**，而是直接测试推导逻辑
//! （`upcoming_reminders` / `next_firing` / 错过补发的落库行为）。
//!
//! 理由：真实循环依赖墙上时钟，测试会变得缓慢且不确定。而最容易出错的
//! 恰恰是"哪些提醒该在什么时候触发"这层纯逻辑 —— 它可以在毫秒内验证。

use super::*;
use crate::domain::task::NewTask;
use crate::repo::reminder_repo::ReminderRepo;
use crate::repo::task_repo::TaskRepo;
use chrono::TimeZone;

fn at(s: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(s).unwrap()
}

fn setup() -> Db {
    Db::open_in_memory().expect("建库失败")
}

/// 建一个带提醒的任务。
fn make_task(
    db: &Db,
    kind: TimeKind,
    due: Option<&str>,
    deadline: Option<&str>,
) -> crate::domain::task::Task {
    let mut input = NewTask::inbox("测试任务");
    input.time_kind = kind;
    input.due_at = due.map(str::to_string);
    input.deadline_at = deadline.map(str::to_string);
    let task = TaskRepo::new(db).create(input).expect("建任务失败");
    ReminderRepo::new(db)
        .create_defaults(&task)
        .expect("建提醒失败");
    task
}

#[test]
fn deadline_task_yields_three_firing_times() {
    let db = setup();
    let task = make_task(
        &db,
        TimeKind::BeforeDeadline,
        None,
        Some("2026-09-30T18:00:00+08:00"),
    );

    // 以"截止前一天"为观察点：三个档位都应还在未来
    let now = at("2026-09-28T12:00:00+08:00");
    let firings = upcoming_reminders(&db, now).unwrap();

    assert_eq!(firings.len(), 3, "截止型任务应有 3 个触发点");
    let times: Vec<String> = firings
        .iter()
        .map(|f| f.scheduled_at.to_rfc3339())
        .collect();
    assert!(
        times.contains(&"2026-09-29T18:00:00+08:00".to_string()),
        "前 1 天"
    );
    assert!(
        times.contains(&"2026-09-30T17:00:00+08:00".to_string()),
        "前 1 小时"
    );
    assert!(
        times.contains(&"2026-09-30T18:00:00+08:00".to_string()),
        "到点"
    );
    assert!(firings.iter().all(|f| f.task_id == task.id));
}

#[test]
fn point_task_yields_single_firing_time() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    let now = at("2026-09-29T12:00:00+08:00");
    let firings = upcoming_reminders(&db, now).unwrap();
    assert_eq!(firings.len(), 1, "时间点任务只到点提醒一次");
    assert_eq!(
        firings[0].scheduled_at.to_rfc3339(),
        "2026-09-30T15:00:00+08:00"
    );
}

/// 全天任务的基准必须被挪到设定钟点，否则提醒会在午夜弹出。
#[test]
fn all_day_task_uses_configured_hour_not_midnight() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AllDay,
        Some("2026-09-30T00:00:00+08:00"),
        None,
    );

    let now = at("2026-09-29T12:00:00+08:00");
    let firings = upcoming_reminders(&db, now).unwrap();

    assert_eq!(firings.len(), 1);
    assert_eq!(
        firings[0].scheduled_at.to_rfc3339(),
        "2026-09-30T09:00:00+08:00",
        "默认应在 09:00 提醒，而不是午夜"
    );
}

/// 这是调度器的核心契约：`next_firing` 只返回**严格未来**的最早一个。
#[test]
fn next_firing_picks_earliest_future() {
    let db = setup();
    make_task(
        &db,
        TimeKind::BeforeDeadline,
        None,
        Some("2026-09-30T18:00:00+08:00"),
    );

    // 观察点落在"截止前 1 小时"之后、"到点"之前
    let now = at("2026-09-30T17:30:00+08:00");
    let next = next_firing(&db, now)
        .unwrap()
        .expect("应有一个未来的触发点");

    assert_eq!(
        next.scheduled_at.to_rfc3339(),
        "2026-09-30T18:00:00+08:00",
        "17:30 时下一个触发点应是 18:00 的到点提醒"
    );
    // 已过去的 09-29 18:00 与前 1 小时不应被选中
    assert!(!next.is_missed);
}

/// 全部提醒都已过期时，`next_firing` 必须返回 None ——
/// 否则调度器会算出一个负的睡眠时长，陷入忙循环。
#[test]
fn next_firing_returns_none_when_all_passed() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    let now = at("2026-10-05T12:00:00+08:00");
    assert!(
        next_firing(&db, now).unwrap().is_none(),
        "全部过期时不应返回任何触发点"
    );
}

/// 已完成的任务不应再产生提醒 —— 否则用户会收到"已经做完的事"的提醒。
#[test]
fn completed_task_produces_no_firing() {
    let db = setup();
    let task = make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    TaskRepo::new(&db).complete(&task.id).unwrap();

    let now = at("2026-09-29T12:00:00+08:00");
    assert!(upcoming_reminders(&db, now).unwrap().is_empty());
}

/// 软删除的任务同样不应触发提醒。
#[test]
fn deleted_task_produces_no_firing() {
    let db = setup();
    let task = make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    TaskRepo::new(&db).soft_delete(&task.id).unwrap();

    let now = at("2026-09-29T12:00:00+08:00");
    assert!(upcoming_reminders(&db, now).unwrap().is_empty());
}

/// 没有时间的任务（收件箱）不应产生提醒。
#[test]
fn inbox_task_produces_no_firing() {
    let db = setup();
    let task = TaskRepo::new(&db)
        .create(NewTask::inbox("随手记的事"))
        .unwrap();
    ReminderRepo::new(&db).create_defaults(&task).unwrap();

    let now = at("2026-09-29T12:00:00+08:00");
    assert!(
        upcoming_reminders(&db, now).unwrap().is_empty(),
        "没有时间基准的任务不该有提醒"
    );
}

/// 关掉通知总开关后，调度器仍应推导出触发点（数据层不受展示层设置影响），
/// 但 `fire()` 会跳过实际弹出。这里只验证推导层不被设置影响。
#[test]
fn notification_switch_does_not_affect_scheduling_math() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    let s = crate::repo::settings_repo::AppSettings {
        notifications_enabled: false,
        ..Default::default()
    };
    crate::repo::settings_repo::SettingsRepo::new(&db)
        .save(&s)
        .unwrap();

    let now = at("2026-09-29T12:00:00+08:00");
    assert_eq!(
        upcoming_reminders(&db, now).unwrap().len(),
        1,
        "通知开关属于展示层，不应改变调度计算"
    );
}

/// 多个任务时，`next_firing` 应跨任务选出全局最早的那个。
#[test]
fn next_firing_is_global_across_tasks() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T20:00:00+08:00"),
        None,
    );
    let early = make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T08:00:00+08:00"),
        None,
    );

    let now = at("2026-09-29T12:00:00+08:00");
    let next = next_firing(&db, now).unwrap().unwrap();

    assert_eq!(next.task_id, early.id, "应选中全局最早的那个");
    assert_eq!(next.scheduled_at.to_rfc3339(), "2026-09-30T08:00:00+08:00");
}

// ===================== 错过补发 =====================

/// 错过补发必须落库，否则用户永远看不到"你错过了什么"。
#[test]
fn catch_up_records_missed_reminders() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    // 观察点在提醒时刻之后（模拟"应用当时没运行"）
    let now = at("2026-09-30T16:00:00+08:00");
    let candidates = upcoming_reminders(&db, now).unwrap();
    let missed: Vec<_> = candidates.iter().filter(|f| f.is_missed).collect();
    assert_eq!(missed.len(), 1, "该提醒应被判定为错过");

    let repo = ReminderRepo::new(&db);
    for m in &missed {
        repo.record_missed(&m.task_id, &m.task_title, &m.scheduled_at)
            .unwrap();
    }

    let pending = repo.pending_missed().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].task_title, "测试任务");
    assert!(!pending[0].is_acknowledged);
}

/// 重复补发不应累加记录 —— 否则每次启动都会多出几条"错过"。
#[test]
fn catch_up_is_idempotent() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-30T15:00:00+08:00"),
        None,
    );

    let now = at("2026-09-30T16:00:00+08:00");
    let repo = ReminderRepo::new(&db);

    // 模拟启动三次
    for _ in 0..3 {
        let candidates = upcoming_reminders(&db, now).unwrap();
        for m in candidates.iter().filter(|f| f.is_missed) {
            repo.record_missed(&m.task_id, &m.task_title, &m.scheduled_at)
                .unwrap();
        }
    }

    assert_eq!(
        repo.pending_missed().unwrap().len(),
        1,
        "多次启动不应产生重复的错过记录"
    );
}

/// 超过 7 天的错过不再补发：一周前该做的事现在弹窗提醒只会造成困扰。
#[test]
fn very_old_missed_reminders_are_not_carried_over() {
    let db = setup();
    make_task(
        &db,
        TimeKind::AtTime,
        Some("2026-09-01T15:00:00+08:00"),
        None,
    );

    let now = at("2026-09-30T12:00:00+08:00");
    let candidates = upcoming_reminders(&db, now).unwrap();

    let within_window: Vec<_> = candidates
        .iter()
        .filter(|f| f.is_missed && (now - f.scheduled_at).num_days() <= 7)
        .collect();

    assert!(
        within_window.is_empty(),
        "29 天前的提醒不应进入补发窗口（它仍留在任务列表里）"
    );
}

/// 时区换算必须保留偏移，不能因为构造过程而丢失。
#[test]
fn to_local_preserves_offset() {
    let naive = chrono::NaiveDate::from_ymd_opt(2026, 9, 30)
        .unwrap()
        .and_hms_opt(15, 0, 0)
        .unwrap();

    let local = to_local(naive).expect("应能构造本地时刻");
    // 与本地时区一致（测试机为 UTC+8 时即为 +08:00）
    let expected_offset = Local
        .with_ymd_and_hms(2026, 9, 30, 15, 0, 0)
        .single()
        .unwrap()
        .offset()
        .local_minus_utc();
    assert_eq!(local.offset().local_minus_utc(), expected_offset);
}

// ===================== 已处理判定与稍后提醒 =====================

/// 生成一个相对当前时刻的 RFC3339 字符串。
///
/// 这些测试必须用**相对**时刻而不能用固定日期：`mark_fired` 写入的是真实
/// 墙钟时间，若计划时刻写死在某个固定日期，两者的先后关系就取决于
/// "测试运行的那一天"，测试会随真实日期漂移。
fn rfc3339_in_minutes(offset: i64) -> String {
    (chrono::Local::now() + chrono::Duration::minutes(offset))
        .fixed_offset()
        .to_rfc3339()
}

/// 这是本轮修掉的一个真实缺陷：已正常触发过的提醒不应再被当成"错过"补发。
#[test]
fn already_fired_reminder_is_not_treated_as_missed() {
    let db = setup();
    // 10 分钟前就该提醒的事
    let due = rfc3339_in_minutes(-10);
    make_task(&db, TimeKind::AtTime, Some(&due), None);

    let now = Local::now().fixed_offset();

    // 触发前：应被判定为错过，且尚未处理过
    let before = upcoming_reminders(&db, now).unwrap();
    assert_eq!(before.len(), 1);
    assert!(before[0].is_missed);
    assert!(!before[0].already_handled(), "尚未触发过，不算是已处理");

    // 模拟正常触发（写入真实当前时间）
    ReminderRepo::new(&db)
        .mark_fired(&before[0].task_id, before[0].offset_seconds)
        .unwrap();

    // 触发后：仍会被推导出来（它是个过去的时刻），但必须被标记为已处理
    let after = upcoming_reminders(&db, now).unwrap();
    assert_eq!(after.len(), 1, "过去的时刻仍会被推导出来");
    assert!(
        after[0].already_handled(),
        "已触发过的档位必须被识别为已处理，否则每次启动都会重复补发"
    );
}

/// 补发必须排除已处理的，这是"你错过了 X 个"数字正确的前提。
#[test]
fn catch_up_skips_already_fired() {
    let db = setup();
    let due = rfc3339_in_minutes(-10);
    let task = make_task(&db, TimeKind::AtTime, Some(&due), None);
    let repo = ReminderRepo::new(&db);
    let now = Local::now().fixed_offset();

    // 先正常触发一次
    repo.mark_fired(&task.id, 0).unwrap();

    // 再走补发筛选逻辑
    let missed: Vec<_> = upcoming_reminders(&db, now)
        .unwrap()
        .into_iter()
        .filter(|r| r.is_missed && !r.already_handled())
        .collect();

    assert!(
        missed.is_empty(),
        "已触发过的提醒不应进入补发集合（否则'错过 3 个'里会混进准时弹过的）"
    );
}

/// 尚未触发过的过去提醒必须进入补发集合。
#[test]
fn never_fired_past_reminder_is_caught_up() {
    let db = setup();
    let due = rfc3339_in_minutes(-10);
    make_task(&db, TimeKind::AtTime, Some(&due), None);
    let now = Local::now().fixed_offset();

    let missed: Vec<_> = upcoming_reminders(&db, now)
        .unwrap()
        .into_iter()
        .filter(|r| r.is_missed && !r.already_handled())
        .collect();

    assert_eq!(missed.len(), 1, "从未触发过的过去提醒应当补发");
}

/// 「稍后提醒」必须取代档位偏移算出的时刻。
#[test]
fn snooze_overrides_tier_offset() {
    let db = setup();
    let due = rfc3339_in_minutes(60);
    let task = make_task(&db, TimeKind::AtTime, Some(&due), None);
    let repo = ReminderRepo::new(&db);

    let snooze_to = rfc3339_in_minutes(5);
    repo.snooze(&task.id, 0, &snooze_to).unwrap();

    let now = Local::now().fixed_offset();
    let firings = upcoming_reminders(&db, now).unwrap();

    assert_eq!(firings.len(), 1);
    assert_eq!(
        firings[0].scheduled_at.to_rfc3339(),
        snooze_to,
        "稍后提醒的时刻应取代原档位时刻"
    );
    assert!(firings[0].snooze_until.is_some());
    assert!(!firings[0].is_missed, "5 分钟后尚未过期");
}

/// 稍后提醒弹出后必须清除，否则会变成无限重复提醒。
#[test]
fn firing_clears_snooze() {
    let db = setup();
    let due = rfc3339_in_minutes(60);
    let task = make_task(&db, TimeKind::AtTime, Some(&due), None);
    let repo = ReminderRepo::new(&db);

    // 稍后提醒的时刻设为 2 分钟前，模拟"这个 snooze 已经到点"
    repo.snooze(&task.id, 0, &rfc3339_in_minutes(-2)).unwrap();

    let now = Local::now().fixed_offset();
    let firing = upcoming_reminders(&db, now)
        .unwrap()
        .into_iter()
        .find(|r| r.task_id == task.id)
        .expect("应能推导出该提醒");
    assert!(firing.snooze_until.is_some(), "该提醒应有待处理的 snooze");

    // 模拟到点触发
    fire(&db, &firing).unwrap();

    assert!(
        repo.snooze_of(&task.id, 0).unwrap().is_none(),
        "触发后必须清除 snooze，否则该时刻永远停在过去，会无限重复提醒"
    );
    assert!(
        repo.tiers_of(&task.id).unwrap()[0].last_fired_at.is_some(),
        "触发后应记录时间"
    );
}

/// 勿扰时段内不弹通知，但仍必须标记为已触发 ——
/// 否则勿扰结束后会一次性补弹一堆，比不弹更烦人。
#[test]
fn quiet_hours_still_marks_fired() {
    let db = setup();
    let due = rfc3339_in_minutes(60);
    let task = make_task(&db, TimeKind::AtTime, Some(&due), None);
    let repo = ReminderRepo::new(&db);

    // 构造一个把当前小时包进勿扰区间的设置
    let current_hour = chrono::Local::now().hour();
    let s = crate::repo::settings_repo::AppSettings {
        quiet_hours_enabled: true,
        quiet_hours_start: current_hour,
        quiet_hours_end: (current_hour + 1) % 24,
        ..Default::default()
    };
    crate::repo::settings_repo::SettingsRepo::new(&db)
        .save(&s)
        .unwrap();

    let firing = FiringReminder {
        task_id: task.id.clone(),
        task_title: task.title.clone(),
        scheduled_at: Local::now().fixed_offset(),
        offset_seconds: 0,
        is_missed: false,
        last_fired_at: None,
        snooze_until: None,
    };
    fire(&db, &firing).unwrap();

    assert!(
        repo.tiers_of(&task.id).unwrap()[0].last_fired_at.is_some(),
        "勿扰时段不弹但仍应标记已触发"
    );
}
