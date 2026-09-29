//! 审计发现的可复现缺陷（每条都对应子代理报告中的一项）。
//!
//! 这些测试**先写来证明缺陷存在**，修复后转为回归测试。
//! 若某条测试在修复前就通过，说明子代理的报告有误，需要重新核实。

use todox_lib::db::connection::Db;
use todox_lib::domain::task::TimeKind;
use todox_lib::repo::reminder_repo::ReminderRepo;
use todox_lib::repo::task_repo::TaskRepo;
use todox_lib::scheduler::{to_local, upcoming_reminders};

fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "todox-repro-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let db = Db::open(&dir).expect("建库失败");
    (db, dir)
}

fn fixed_now() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 9, 29)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap()
}

// ============================================================================
// 发现 1（严重）：自然语言创建的全天任务永远不会有提醒
// ============================================================================

/// `明天 买牛奶` 这类"只有日期、没有时刻"的输入，落库时 `due_at` 是
/// `NaiveDate::to_string()` 的产物 `"2026-10-01"`（10 字节、无时间部分）。
///
/// 而 `attach_local_offset` 只认两种带时间的格式，因此这个值被原样透传；
/// 调度器用 `DateTime::parse_from_rfc3339` 解析它时因长度不足而失败，
/// 于是 `continue` 跳过 —— 任务永远不会触发任何提醒。
///
/// 这是最常见的输入形态之一（"明天要做X"），因此定为严重。
#[test]
fn repro_1_all_day_task_from_text_never_reminds() {
    let (db, dir) = temp_db("allday");
    let now = fixed_now();

    let task = todox_lib::commands::create_from_text_with(&db, "明天 买牛奶", now).unwrap();

    assert_eq!(task.time_kind, TimeKind::AllDay);

    // 打印实际落库的格式，便于确认问题所在
    let due = task.due_at.clone().unwrap_or_default();
    println!("due_at 实际值 = {due:?}（长度 {}）", due.len());

    // 提醒档位确实被创建了
    let tiers = ReminderRepo::new(&db).tiers_of(&task.id).unwrap();
    assert!(!tiers.is_empty(), "档位应当被创建");

    // **关键断言**：调度器必须能从这个任务推导出触发点
    let firings = upcoming_reminders(&db, to_local(now).unwrap()).unwrap();
    let mine: Vec<_> = firings.iter().filter(|f| f.task_id == task.id).collect();

    assert!(
        !mine.is_empty(),
        "全天任务必须能产生提醒触发点。\
         实际 due_at = {due:?}，推导结果 = {firings:?}。\
         若为空，说明该任务永远不会提醒 —— 用户看到任务带日期地躺在列表里，但从不响。"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 对照：手写一个格式正确的全天时间，调度器应能正常推导。
/// 这能区分"全天逻辑本身坏了"还是"日期字符串格式不对"。
#[test]
fn repro_1b_all_day_with_proper_format_works() {
    let (db, dir) = temp_db("allday-ok");
    let now = fixed_now();

    // 用一个带时区偏移的完整 RFC3339 日期时间
    let mut input = todox_lib::domain::task::NewTask::inbox("手写全天任务");
    input.time_kind = TimeKind::AllDay;
    input.due_at = Some("2026-10-01T00:00:00+08:00".into());

    let task = TaskRepo::new(&db).create(input).unwrap();
    ReminderRepo::new(&db).create_defaults(&task).unwrap();

    let firings = upcoming_reminders(&db, to_local(now).unwrap()).unwrap();
    let mine: Vec<_> = firings.iter().filter(|f| f.task_id == task.id).collect();

    assert!(!mine.is_empty(), "格式正确的全天时间应当能推导出触发点");

    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// 发现 2（严重）：用户输入可触发 chrono panic（release 下 abort 整个进程）
// ============================================================================

/// 超大数值的时长表达会让 `NaiveDateTime + TimeDelta` 溢出并 panic。
///
/// 严重性来自两点：
///   1. `panic = "abort"`，release 下直接杀进程 —— 托盘常驻与所有提醒一起消失；
///   2. 前端在**输入停顿 180ms 后**就会调 `parse_input`，
///      因此用户还没点保存、只是在打字或粘贴，进程就可能没了。
#[test]
fn repro_2_huge_duration_must_not_panic() {
    let (db, dir) = temp_db("huge");
    let now = fixed_now();

    // 这些输入若触发 panic，测试进程会直接挂掉 —— 那正是要证明的
    let cases = [
        "13800138000小时后 回电",
        "99999999天后 取货",
        "99999999999999天后 取货",
        "999999999999999999分钟 后 开会",
    ];

    for c in cases {
        let r = todox_lib::commands::create_from_text_with(&db, c, now);
        // 不关心成功还是返回错误，只要求**不能 panic**
        println!("输入 {c:?} -> {r:?}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `parse_input`（预览路径）同样不能 panic —— 它在用户打字时就会被调用。
#[test]
fn repro_2b_preview_must_not_panic_on_huge_values() {
    let now = fixed_now();
    for c in [
        "13800138000小时后 回电",
        "99999999天后 取货",
        "99999999999999999999秒后 x",
    ] {
        let text = c.to_string();
        // build_preview 是 parse_input 命令的实现，不落库
        let p = todox_lib::nlp::parse(&text, now);
        // 只要求不 panic；解析出什么值不重要
        let _ = p.offset_seconds;
    }
}

// ============================================================================
// 发现 3（中等）：「合并」导入会静默删除本地子表数据
// ============================================================================

/// `INSERT OR REPLACE` 在同 ID 冲突时会**真的 DELETE 旧行**，
/// 从而触发 `ON DELETE CASCADE`，把本地关联的完成记录与提醒档位删掉。
///
/// 关键在于"**本地独有**"的关联数据：备份文件里没有它们，
/// 但它们在本地确实存在（例如备份之后用户又完成了两轮）。
/// 合并导入时它们会被级联删除，且用户完全无从察觉。
///
/// 而前端的确认对话框里「取消」按钮对应的正是"合并"路径，
/// 文案写的是「按 ID 合并，两条都保留」—— 与实际行为不符。
#[test]
fn repro_3_merge_import_must_not_delete_local_children() {
    use todox_lib::repo::export_repo::{export, import};

    let (src, src_dir) = temp_db("merge-src");
    let (dst, dst_dir) = temp_db("merge-dst");
    let now = fixed_now();

    // 源库：一个重复任务 + 一轮完成
    let src_task = todox_lib::commands::create_from_text_with(&src, "每天 8:00 吃药", now).unwrap();
    TaskRepo::new(&src).complete(&src_task.id).unwrap();

    let envelope = export(&src).unwrap();

    // 目标库先导入一次，得到同 ID 的任务
    import(&dst, &envelope, true).unwrap();

    // 关键：在目标库上再**本地**完成两轮 —— 这些记录不在备份文件里
    TaskRepo::new(&dst).complete(&src_task.id).unwrap();
    TaskRepo::new(&dst).complete(&src_task.id).unwrap();

    let children_before = TaskRepo::new(&dst).completions_of(&src_task.id).unwrap();
    let tiers_before = ReminderRepo::new(&dst).tiers_of(&src_task.id).unwrap();
    assert!(
        children_before.len() >= 2,
        "前置条件：本地应有额外的完成记录，实际 {} 条",
        children_before.len()
    );
    assert!(!tiers_before.is_empty(), "前置条件：应有提醒档位");

    // 以「合并」模式再次导入同一个信封
    import(&dst, &envelope, false).unwrap();

    let children_after = TaskRepo::new(&dst).completions_of(&src_task.id).unwrap();
    let tiers_after = ReminderRepo::new(&dst).tiers_of(&src_task.id).unwrap();

    assert_eq!(
        children_after.len(),
        children_before.len(),
        "「合并」模式不应删除**本地独有**的完成记录。\
         INSERT OR REPLACE 会真的 DELETE 旧行并触发 ON DELETE CASCADE。\
         导入前 {} 条，导入后 {} 条",
        children_before.len(),
        children_after.len()
    );
    assert_eq!(
        tiers_after.len(),
        tiers_before.len(),
        "「合并」模式不应删除本地的提醒档位"
    );

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

/// 替换一条 `recurrence_rule` 不应把引用它的任务变成"重复任务却没有规则"。
///
/// `recurrence_rule` 的外键是 `ON DELETE SET NULL`，因此
/// `INSERT OR REPLACE` 在替换规则行时会把 `task.recurrence_id` 置为 NULL，
/// 留下一个 `time_kind = 'recurring'` 却无规则的任务 —— 之后完成它会报
/// "重复任务缺少重复规则"，用户无法理解也无法自行修复。
#[test]
fn repro_3b_merge_import_must_not_orphan_recurrence_reference() {
    use todox_lib::repo::export_repo::{export, import};

    let (src, src_dir) = temp_db("merge-rule-src");
    let (dst, dst_dir) = temp_db("merge-rule-dst");
    let now = fixed_now();

    let src_task =
        todox_lib::commands::create_from_text_with(&src, "每周一 9:00 交周报", now).unwrap();
    assert!(src_task.recurrence_id.is_some(), "前置条件：应有重复规则");

    let envelope = export(&src).unwrap();
    import(&dst, &envelope, true).unwrap();

    // 再次合并导入
    import(&dst, &envelope, false).unwrap();

    let after = TaskRepo::new(&dst).get(&src_task.id).unwrap().unwrap();
    assert!(
        after.recurrence_id.is_some(),
        "合并导入后任务仍应保有重复规则引用。\
         recurrence_id 变成 NULL 会让它成为'重复任务却没有规则'的坏状态"
    );

    // 并且它必须仍然可以正常完成（不会报"缺少重复规则"）
    let r = TaskRepo::new(&dst).complete(&src_task.id);
    assert!(r.is_ok(), "任务应仍可正常完成，实际：{r:?}");

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

// ============================================================================
// 发现 4（中等）：reset_defaults 不是原子的
// ============================================================================

/// `reset_defaults` 先在一个自动提交的语句里软删旧档位，
/// 再在另一个事务里创建新档位。两步之间失败会让任务**永久失去全部提醒**。
///
/// 这里无法轻易注入失败，因此改为验证"两步之间档位为空的窗口存在"：
/// 直接调用软删那一步，确认此时任务确实没有任何有效档位。
#[test]
fn repro_4_reset_defaults_has_a_window_with_no_tiers() {
    let (db, dir) = temp_db("reset-window");
    let now = fixed_now();

    let task = todox_lib::commands::create_from_text_with(&db, "每天 8:00 吃药", now).unwrap();
    let repo = ReminderRepo::new(&db);
    assert!(!repo.tiers_of(&task.id).unwrap().is_empty());

    // 模拟 reset_defaults 的第一步（软删）之后、第二步之前的瞬间状态。
    // 这个"瞬间"在真实实现里是两个独立的事务，中间失败即永久无提醒。
    {
        let conn = db.lock().unwrap();
        conn.execute(
            "UPDATE reminder SET deleted_at = ?2 WHERE task_id = ?1 AND deleted_at IS NULL",
            rusqlite::params![task.id, "2026-01-01T00:00:00+08:00"],
        )
        .unwrap();
    }

    let tiers = repo.tiers_of(&task.id).unwrap();
    assert!(
        tiers.is_empty(),
        "前置条件：软删后应无有效档位（证明这个中间状态确实存在）"
    );

    // 此时若 create_defaults 失败，任务就永远没有提醒了。
    // 修复方向是让两步处于同一事务，届时这个中间状态在外部不可观测。
    let firings = upcoming_reminders(&db, to_local(now).unwrap()).unwrap();
    assert!(
        firings.iter().all(|f| f.task_id != task.id),
        "无档位时不应有触发点（说明这个中间状态确实会导致漏提醒）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
