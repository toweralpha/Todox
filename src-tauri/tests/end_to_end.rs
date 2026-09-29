//! 端到端集成测试：从自然语言文本到真实数据库落盘。
//!
//! 与各模块的单元测试不同，这里刻意使用**真实的文件数据库**而非内存库，
//! 目的是覆盖单元测试碰不到的东西：
//!   - WAL 模式与文件读写
//!   - 迁移在真实文件上的幂等性
//!   - 重新打开数据库后数据确实还在（真正的持久化）
//!
//! 所有断言都基于**固定的 `now`**，因此结果不随真实日期漂移。

use chrono::NaiveDate;
use todox_lib::commands::create_from_text_with;
use todox_lib::db::connection::Db;
use todox_lib::domain::task::TimeKind;
use todox_lib::repo::task_repo::TaskRepo;

/// 参考时刻：2026-09-29（周二）14:00。
fn fixed_now() -> chrono::NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 9, 29)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap()
}

/// 建一个临时目录下的真实数据库。
///
/// 每个测试用独立目录，避免并行执行时互相干扰。
fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "todox-e2e-{}-{}-{}",
        tag,
        std::process::id(),
        // 用时间戳避免同一次运行内重复
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).expect("建库失败");
    (db, dir)
}

#[test]
fn full_flow_creates_tasks_with_correct_time_kinds() {
    let (db, dir) = temp_db("kinds");
    let now = fixed_now();
    let repo = TaskRepo::new(&db);

    // 时间点任务
    let t1 = create_from_text_with(&db, "明天下午3点 交房租", now).unwrap();
    assert_eq!(t1.title, "交房租");
    assert_eq!(t1.time_kind, TimeKind::AtTime);
    assert!(
        t1.due_at
            .as_deref()
            .unwrap()
            .starts_with("2026-09-30T15:00"),
        "时间点应为明天 15:00，实际：{:?}",
        t1.due_at
    );

    // 全天任务
    let t2 = create_from_text_with(&db, "后天 买牛奶", now).unwrap();
    assert_eq!(t2.title, "买牛奶");
    assert_eq!(t2.time_kind, TimeKind::AllDay);
    assert!(
        t2.due_at.as_deref().unwrap().starts_with("2026-10-01"),
        "全天任务应为 10-01，实际：{:?}",
        t2.due_at
    );

    // 收件箱（无时间）
    let t3 = create_from_text_with(&db, "整理书桌", now).unwrap();
    assert_eq!(t3.title, "整理书桌");
    assert!(t3.due_at.is_none());
    assert!(t3.deadline_at.is_none());

    // 相对时长
    let t4 = create_from_text_with(&db, "3小时后 取快递", now).unwrap();
    assert_eq!(t4.title, "取快递");
    assert!(
        t4.due_at
            .as_deref()
            .unwrap()
            .starts_with("2026-09-29T17:00"),
        "3 小时后应为 17:00，实际：{:?}",
        t4.due_at
    );

    // 全部四次创建都应留下 changelog（同步所需的副作用不能被漏掉）
    let conn = db.lock().unwrap();
    let logged: i64 = conn
        .query_row("SELECT COUNT(*) FROM changelog", [], |r| r.get(0))
        .unwrap();
    assert_eq!(logged, 4, "每次创建都应写入一条变更流水");
    drop(conn);

    assert_eq!(repo.list_all(false).unwrap().len(), 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn recurring_task_gets_rule_and_advances_on_completion() {
    let (db, dir) = temp_db("recurring");
    let now = fixed_now();
    let repo = TaskRepo::new(&db);

    let task = create_from_text_with(&db, "每周一早上9点 提交周报", now).unwrap();
    assert_eq!(task.title, "提交周报");
    assert_eq!(task.time_kind, TimeKind::Recurring);
    assert!(task.recurrence_id.is_some(), "应有重复规则");
    // 2026-09-29 是周二，本周一已过 → 首次发生应为下周一 10-05
    assert!(
        task.due_at
            .as_deref()
            .unwrap()
            .starts_with("2026-10-05T09:00"),
        "首次发生应为下周一 09:00，实际：{:?}",
        task.due_at
    );

    // 完成这一轮：任务本身不应变成已完成，而是推进到下一周
    repo.complete(&task.id).unwrap();

    let after = repo.get(&task.id).unwrap().unwrap();
    assert!(
        !after.is_completed,
        "重复任务勾选后不能变成已完成，否则整条序列就结束了"
    );
    assert!(
        after
            .due_at
            .as_deref()
            .unwrap()
            .starts_with("2026-10-12T09:00"),
        "应推进到再下一周的周一，实际：{:?}",
        after.due_at
    );
    assert_eq!(repo.completions_of(&task.id).unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 关键验证：关掉数据库再打开，数据必须还在。
/// 这是"待办不会丢"这一承诺的唯一真实验证方式。
#[test]
fn data_survives_reopen() {
    let dir = std::env::temp_dir().join(format!("todox-e2e-reopen-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let now = fixed_now();

    {
        let db = Db::open(&dir).expect("首次建库失败");
        create_from_text_with(&db, "明天下午3点 交房租", now).unwrap();
        create_from_text_with(&db, "每天 8:00 吃维生素", now).unwrap();
        // 离开作用域时连接被释放
    }

    {
        let db = Db::open(&dir).expect("重新打开失败");
        let repo = TaskRepo::new(&db);
        let tasks = repo.list_all(false).unwrap();

        assert_eq!(tasks.len(), 2, "重新打开后任务数量应保持");
        let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"交房租"));
        assert!(titles.contains(&"吃维生素"));

        // 并且两次打开时的 user_version 必须一致，说明迁移是幂等的。
        // 引用常量而不是写死数字：否则每次新增 schema 版本都要改这行，
        // 而漏改的表现是一个与本次改动毫无关系的测试失败。
        let conn = db.lock().unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            ver,
            todox_lib::db::schema::SCHEMA_VERSION,
            "重新打开后 schema 版本应是最新版（迁移幂等）"
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 解析失败时绝不能丢失用户输入 —— 这是硬性要求。
#[test]
fn unparseable_text_is_still_saved_verbatim() {
    let (db, dir) = temp_db("verbatim");
    let now = fixed_now();

    for input in ["我买了3个苹果", "整理书桌", "5次会议"] {
        let t = create_from_text_with(&db, input, now).unwrap();
        assert_eq!(t.title, input, "「{input}」的标题被改动了");
        assert!(t.due_at.is_none(), "「{input}」不应产生时间");
        assert!(t.recurrence_id.is_none(), "「{input}」不应产生重复规则");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// 只输入时间表达时，用时间原文作为标题，而不是拒绝创建。
#[test]
fn time_only_input_becomes_title() {
    let (db, dir) = temp_db("timeonly");
    let now = fixed_now();

    let t = create_from_text_with(&db, "明天下午3点", now).unwrap();
    assert!(!t.title.is_empty(), "只输入时间不应被拒绝");
    assert_eq!(t.time_kind, TimeKind::AtTime);
    assert!(t.due_at.is_some());

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn empty_input_is_rejected() {
    let (db, dir) = temp_db("empty");
    let now = fixed_now();

    assert!(create_from_text_with(&db, "", now).is_err());
    assert!(create_from_text_with(&db, "   ", now).is_err());

    let _ = std::fs::remove_dir_all(&dir);
}

/// 删除后撤销，数据应完整回来。这条链路依赖软删除设计。
#[test]
fn delete_then_restore_roundtrip() {
    let (db, dir) = temp_db("restore");
    let now = fixed_now();
    let repo = TaskRepo::new(&db);

    let t = create_from_text_with(&db, "明天 开会", now).unwrap();
    repo.soft_delete(&t.id).unwrap();
    assert_eq!(repo.list_all(false).unwrap().len(), 0);

    let restored = repo.restore(&t.id).unwrap();
    assert_eq!(restored.id, t.id);
    assert_eq!(restored.title, "开会");
    assert_eq!(repo.list_all(false).unwrap().len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

// ===================== 导入导出 =====================

/// 导出再导入到**另一个真实文件数据库**，数据必须完整回来。
///
/// 用两个不同的真实文件而不是内存库：这一步要覆盖的正是"写到磁盘再读回来"
/// 这条路径，内存库无法验证文件层面的任何问题。
#[test]
fn export_import_between_real_files() {
    use todox_lib::repo::export_repo::{export, import, ExportEnvelope};

    let (src, src_dir) = temp_db("export-src");
    let now = fixed_now();

    // 造一批有代表性的数据：普通任务、时间点任务、重复任务、完成记录
    create_from_text_with(&src, "整理书桌", now).unwrap();
    create_from_text_with(&src, "明天下午3点 交房租", now).unwrap();
    let recurring = create_from_text_with(&src, "每周一早上9点 提交周报", now).unwrap();
    TaskRepo::new(&src).complete(&recurring.id).unwrap();

    // 此刻共 3 个任务（含 1 个已推进的重复任务）
    assert_eq!(TaskRepo::new(&src).list_all(true).unwrap().len(), 3);

    // 导出 → 序列化为 JSON 文本 → 再解析回来 → 导入到新库。
    // 中间经过真实的 JSON 文本往返，才能覆盖用户实际使用的路径。
    let envelope = export(&src).unwrap();
    let json = serde_json::to_string(&envelope).unwrap();

    // 以**导出时刻**的状态为基准。
    // 注意不能在 complete() 之前取基准：重复任务完成时它的 due_at 会被推进，
    // 用旧的基准去比对"导入后是否一致"必然失败。
    let exported_state = TaskRepo::new(&src).list_all(true).unwrap();

    let (dst, dst_dir) = temp_db("export-dst");
    let parsed: ExportEnvelope = serde_json::from_str(&json).unwrap();
    let summary = import(&dst, &parsed, true).unwrap();

    assert!(summary.total >= 3, "至少应导入 3 个任务");

    let after = TaskRepo::new(&dst).list_all(true).unwrap();
    assert_eq!(after.len(), 3);

    // ID 必须完全一致 —— 否则完成记录会与新任务失去关联
    let mut ids_before: Vec<&str> = exported_state.iter().map(|t| t.id.as_str()).collect();
    let mut ids_after: Vec<&str> = after.iter().map(|t| t.id.as_str()).collect();
    ids_before.sort();
    ids_after.sort();
    assert_eq!(ids_before, ids_after, "导出再导入后任务 ID 必须一致");

    // 重复任务被推进后的发生时间应原样保留（而不是被重置回首次发生时间）
    let rec_exported = exported_state
        .iter()
        .find(|t| t.id == recurring.id)
        .expect("导出状态里应有该重复任务");
    let rec_after = after
        .iter()
        .find(|t| t.id == recurring.id)
        .expect("导入后应存在该重复任务");
    assert_eq!(
        rec_after.due_at, rec_exported.due_at,
        "推进后的发生时间应原样保留"
    );
    assert_ne!(
        rec_after.due_at, recurring.due_at,
        "该任务在此之前已被完成过一轮，时间应已推进"
    );

    // 完成记录也必须跟着过来，否则统计会从零开始
    let completions = TaskRepo::new(&dst).completions_of(&recurring.id).unwrap();
    assert_eq!(completions.len(), 1, "完成记录应随导出一起迁移");

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

/// 统计接口在真实数据上的正确性。
#[test]
fn stats_reflect_real_completions() {
    use todox_lib::repo::stats_repo::StatsRepo;

    let (db, dir) = temp_db("stats");
    let now = fixed_now();

    // 两个每日重复任务，分别完成 4 次与 2 次
    let a = create_from_text_with(&db, "每天 8:00 吃维生素", now).unwrap();
    let b = create_from_text_with(&db, "每天 9:00 背单词", now).unwrap();

    for _ in 0..4 {
        TaskRepo::new(&db).complete(&a.id).unwrap();
    }
    for _ in 0..2 {
        TaskRepo::new(&db).complete(&b.id).unwrap();
    }

    let repo = StatsRepo::new(&db);
    let overview = repo.overview().unwrap();

    assert_eq!(overview.total_completions, 6, "4 + 2 次完成");
    assert_eq!(overview.today_completions, 6);
    assert_eq!(overview.unfinished, 2, "两个重复任务本身仍未完成");

    let streaks = repo.by_task(10).unwrap();
    assert_eq!(streaks.len(), 2);
    assert_eq!(streaks[0].task_title, "吃维生素", "完成次数多的排前面");
    assert_eq!(streaks[0].total, 4);
    assert_eq!(streaks[1].total, 2);

    // 每日统计必须覆盖完整天数（含没有记录的日子），否则柱状图会误导
    let daily = repo.daily_counts(7).unwrap();
    assert_eq!(daily.len(), 7);
    assert_eq!(daily.last().unwrap().count, 6, "今天共完成 6 次");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 把任务改成截止型任务，并验证它按新类型获得三级提醒档位。
///
/// 这是阶段五新增的界面能力：自然语言解析器不产生截止型任务，
/// 用户必须能通过编辑改过来，且提醒档位要跟着变 ——
/// 否则"分级提醒"这套能力永远无法从界面触达。
#[test]
fn editing_to_deadline_task_gets_three_tiers() {
    use todox_lib::domain::task::{TaskEdit, TimeKind};
    use todox_lib::repo::reminder_repo::ReminderRepo;
    use todox_lib::repo::settings_repo::{AppSettings, SettingsRepo};

    let (db, dir) = temp_db("edit-deadline");
    let now = fixed_now();

    // 先建一个收件箱任务：默认只有"到点"一个提醒档位
    let t = create_from_text_with(&db, "整理书桌", now).unwrap();
    let reminder_repo = ReminderRepo::new(&db);
    assert_eq!(
        reminder_repo.tiers_of(&t.id).unwrap().len(),
        1,
        "普通任务默认只有到点提醒"
    );

    // 通过编辑改成截止型，并给出截止时间
    let edit = TaskEdit {
        time_kind: Some(TimeKind::BeforeDeadline),
        deadline_at: Some(Some("2026-10-01T18:00:00+08:00".into())),
        ..Default::default()
    };
    let updated = TaskRepo::new(&db).update(&t.id, &edit).unwrap();
    assert_eq!(updated.time_kind, TimeKind::BeforeDeadline);

    // 模拟命令层的做法：类型变了就重建档位
    let settings = SettingsRepo::new(&db).load().unwrap();
    reminder_repo.reset_defaults(&updated).unwrap();

    let tiers = reminder_repo.tiers_of(&updated.id).unwrap();
    assert_eq!(tiers.len(), 3, "截止型任务应有三级提醒");
    let offsets: Vec<i64> = tiers.iter().map(|t| t.offset_seconds).collect();
    assert_eq!(offsets, vec![-86400, -3600, 0], "应为前1天/前1小时/到点");

    // 确认产品默认策略与设置一致，防止两者脱节
    assert_eq!(AppSettings::default().deadline_offsets_seconds, offsets);
    assert_eq!(
        settings.deadline_offsets_seconds, offsets,
        "设置里也应是三级"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 设置必须能跨重启保留 —— 用户不希望每次开机都重设一遍。
#[test]
fn settings_persist_across_reopen() {
    use todox_lib::repo::settings_repo::{AppSettings, SettingsRepo};

    let dir = std::env::temp_dir().join(format!("todox-e2e-settings-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);

    let custom = AppSettings {
        notifications_enabled: false,
        quiet_hours_enabled: true,
        quiet_hours_start: 23,
        quiet_hours_end: 7,
        deadline_offsets_seconds: vec![-7200, 0],
        point_offsets_seconds: vec![-300, 0],
        all_day_hour: 8,
        all_day_minute: 30,
        close_to_tray: false,
        theme: "dark".into(),
        snooze_minutes: 25,
    };

    {
        let db = Db::open(&dir).expect("首次建库失败");
        SettingsRepo::new(&db).save(&custom).unwrap();
    }

    {
        let db = Db::open(&dir).expect("重新打开失败");
        let loaded = SettingsRepo::new(&db).load().unwrap();
        assert_eq!(loaded, custom, "设置应完整跨重启保留");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ===================== 秒级提醒（用户实际反馈的场景）=====================

/// 「10秒后提醒我」必须真正安排出一个 10 秒后触发的提醒。
///
/// 这条测试对应一个真实的用户反馈："我输入 10 秒后提醒我，结果没有反馈"。
/// 根因有两层：
///   1. 解析器的时长单位表里根本没有「秒」，于是这句话被当成了
///      **没有时间的普通任务**存进收件箱 —— 它压根没有安排任何提醒；
///   2. 即使解析出了时长，内部单位是分钟，10 秒也会被截断成 0。
///
/// 因此这条测试检查的不只是"解析器认识秒"，而是**整条链路**：
/// 解析 → 建任务 → 用任务的 time_kind 建默认提醒档位 → 调度器推导出触发时刻。
#[test]
fn ten_seconds_later_actually_schedules_a_reminder() {
    use todox_lib::repo::reminder_repo::ReminderRepo;
    use todox_lib::scheduler::upcoming_reminders;

    let (db, dir) = temp_db("seconds");
    let now = fixed_now();

    let task = create_from_text_with(&db, "10秒后 提醒我", now).unwrap();

    // 第一层：必须被识别成"有时间"的任务，而不是落进收件箱
    assert_eq!(
        task.time_kind,
        TimeKind::AtTime,
        "「10秒后」应被识别成时间点任务，而不是没有时间的收件箱任务"
    );
    assert_eq!(task.title, "提醒我", "时间表达应从标题中剥离");

    // 第二层：时间必须精确到 10 秒之后，不能被截断成 0
    let due = task.due_at.as_deref().expect("必须有时间");
    let due_at = chrono::DateTime::parse_from_rfc3339(due).expect("应是合法的 RFC3339");
    let delta = due_at.naive_local() - now;
    assert_eq!(
        delta.num_seconds(),
        10,
        "触发时刻必须恰好是 10 秒后，实际差 {} 秒（due={due}）",
        delta.num_seconds()
    );

    // 第三层：调度器必须能从这条任务推导出一个真实的提醒触发点
    let firing = upcoming_reminders(
        &db,
        todox_lib::scheduler::to_local(now).expect("应能构造本地时刻"),
    )
    .expect("调度器应能推导出待触发提醒");
    let task_ids: Vec<&str> = firing.iter().map(|f| f.task_id.as_str()).collect();
    assert!(
        task_ids.contains(&task.id.as_str()),
        "调度器的待触发集合里应包含这条任务"
    );

    // 且该触发点就在 10 秒后附近（允许秒级舍入误差）
    let my_firing = firing
        .iter()
        .find(|f| f.task_id == task.id)
        .expect("应能找到该任务的触发点");
    let fire_delta = my_firing.scheduled_at.naive_local() - now;
    assert!(
        (fire_delta.num_seconds() - 10).abs() <= 1,
        "触发点应在 10 秒后附近，实际 {} 秒",
        fire_delta.num_seconds()
    );

    // 提醒档位也必须真的落库了 —— 没有档位就不会有任何提醒
    let tiers = ReminderRepo::new(&db).tiers_of(&task.id).unwrap();
    assert!(!tiers.is_empty(), "必须为任务创建提醒档位");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 秒级时长不能把「30秒钟后」算成 3 秒。
#[test]
fn thirty_seconds_with_zhong_unit_is_not_truncated() {
    let (db, dir) = temp_db("seconds-zhong");
    let now = fixed_now();

    let task = create_from_text_with(&db, "30秒钟后 检查", now).unwrap();
    let due = chrono::DateTime::parse_from_rfc3339(task.due_at.as_deref().unwrap()).unwrap();
    let delta = (due.naive_local() - now).num_seconds();

    assert_eq!(
        delta, 30,
        "「30秒钟后」是 30 秒而不是 3 秒 —— 数字与单位之间夹着另一个单位字时容易截错"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
