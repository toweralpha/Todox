//! 攻击场景审计：恶意导入文件能否破坏用户数据。
//!
//! 这些测试的目标不是"证明代码安全"，而是**尝试真正破坏它**。
//! 如果某个测试失败，那就是一个需要修的真实漏洞。

use todox_lib::db::connection::Db;
use todox_lib::domain::task::{NewTask, TimeKind};
use todox_lib::repo::export_repo::{export, import, ExportEnvelope};
use todox_lib::repo::task_repo::TaskRepo;

fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "todox-attack-{}-{}-{}",
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

fn seed(db: &Db, n: usize) {
    let repo = TaskRepo::new(db);
    for i in 0..n {
        let mut t = NewTask::inbox(format!("用户数据 {i}"));
        t.time_kind = TimeKind::AllDay;
        repo.create(t).unwrap();
    }
}

fn task_count(db: &Db) -> i64 {
    TaskRepo::new(db)
        .table_counts()
        .unwrap()
        .into_iter()
        .find(|(t, _)| t == "task")
        .map(|(_, c)| c)
        .unwrap()
}

/// 核心攻击：先用 `replace_existing = true` 触发"清空"，再让导入中途失败。
///
/// 如果事务没有正确回滚，用户的数据会被**清空且无法恢复** ——
/// 这是这个应用能发生的最严重的后果，比任何注入都严重。
#[test]
fn attack_failed_import_after_clear_must_roll_back() {
    let (db, dir) = temp_db("rollback");
    seed(&db, 5);
    assert_eq!(task_count(&db), 5);

    // 让导入**必定失败**：写入一个违反 NOT NULL 约束的行。
    // 未知列现在会被安全过滤（那是有意的设计），因此不能用未知列来制造失败。
    let mut envelope: ExportEnvelope = export(&db).unwrap();
    let mut bad_row = serde_json::Map::new();
    // 只给 is_completed，故意不给 NOT NULL 的 title / time_kind 等
    bad_row.insert("is_completed".into(), serde_json::Value::from(1));
    envelope
        .tables
        .insert("task".into(), vec![serde_json::Value::Object(bad_row)]);

    let result = import(&db, &envelope, true);

    assert!(result.is_err(), "违反约束的行应导致导入失败");
    assert_eq!(
        task_count(&db),
        5,
        "**导入失败后用户数据必须完好无损**。\
         若这里是 0，说明清空语句已提交而导入失败，用户数据被永久销毁 —— 严重漏洞"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 中间某一张表失败时，之前已成功写入的表也必须回滚。
#[test]
fn attack_partial_failure_must_not_leave_partial_data() {
    let (db, dir) = temp_db("partial");

    let mut envelope: ExportEnvelope = export(&db).unwrap();

    // task 表写入一条合法数据
    let mut good = serde_json::Map::new();
    good.insert(
        "id".into(),
        serde_json::Value::from("aaaaaaaa-0000-0000-0000-000000000001"),
    );
    good.insert("title".into(), serde_json::Value::from("注入写入的数据"));
    good.insert("time_kind".into(), serde_json::Value::from("all_day"));
    good.insert("priority".into(), serde_json::Value::from(0));
    good.insert("is_completed".into(), serde_json::Value::from(0));
    good.insert("sort_order".into(), serde_json::Value::from(0));
    good.insert(
        "created_at".into(),
        serde_json::Value::from("2026-01-01T00:00:00+08:00"),
    );
    good.insert(
        "updated_at".into(),
        serde_json::Value::from("2026-01-01T00:00:00+08:00"),
    );
    good.insert("revision".into(), serde_json::Value::from(1));
    envelope
        .tables
        .insert("task".into(), vec![serde_json::Value::Object(good)]);

    // tag 表（在 task 之后处理）写入一条违反 NOT NULL 的行（缺 name）
    let mut bad = serde_json::Map::new();
    bad.insert(
        "id".into(),
        serde_json::Value::from("bbbbbbbb-0000-0000-0000-000000000002"),
    );
    envelope
        .tables
        .insert("tag".into(), vec![serde_json::Value::Object(bad)]);

    let result = import(&db, &envelope, true);
    assert!(result.is_err(), "缺 NOT NULL 列的行应导致整体失败");
    assert_eq!(
        task_count(&db),
        0,
        "部分写入必须随事务一起回滚，否则数据库会处于半新半旧的错乱状态"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 列名里带 SQL 语法时，无论 SQLite 最终报什么错，
/// **用户数据都不能被破坏**。这个测试不关心错误信息，只关心数据完整性。
#[test]
fn attack_injection_payloads_cannot_destroy_data() {
    let payloads = [
        "x) VALUES (NULL); DELETE FROM task; --",
        "id) VALUES ('a','b','c','all_day',NULL,NULL,NULL,0,0,NULL,0,'2026-01-01T00:00:00+08:00','2026-01-01T00:00:00+08:00',NULL,1); DELETE FROM task; --",
        "1; DROP TABLE task; --",
        "(SELECT 1) --",
        "id` , `title",
    ];

    for (i, p) in payloads.iter().enumerate() {
        let (db, dir) = temp_db(&format!("payload{i}"));
        seed(&db, 3);

        let mut envelope: ExportEnvelope = export(&db).unwrap();
        let mut row = serde_json::Map::new();
        row.insert(p.to_string(), serde_json::Value::from(1));
        envelope
            .tables
            .insert("task".into(), vec![serde_json::Value::Object(row)]);

        // 不论成功与否，重要的是数据完整性
        let _ = import(&db, &envelope, false);

        assert_eq!(
            task_count(&db),
            3,
            "注入载荷 {p:?} 导致用户数据被破坏（期望仍有 3 条）"
        );

        // 表结构也必须完好
        let conn = db.lock().unwrap();
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='task'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1, "载荷 {p:?} 删除或篡改了 task 表结构");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 导入文件里出现 schema 中不存在的列时，**必须把它汇报给用户**。
///
/// 设计取舍：未知列会被丢弃并继续导入，而不是让整个文件无法恢复 ——
/// 备份可能来自更新版本，能恢复大部分数据比什么都恢复不了要好。
/// 但"丢弃"必须让用户知道，否则就是静默的数据丢失。
#[test]
fn unknown_columns_are_reported_not_silently_dropped() {
    let (db, dir) = temp_db("errmsg");

    let mut envelope: ExportEnvelope = export(&db).unwrap();
    let mut row = serde_json::Map::new();
    // 一个真实存在的列 + 一个不存在的列
    row.insert(
        "id".into(),
        serde_json::Value::from("cccccccc-0000-0000-0000-000000000003"),
    );
    row.insert("title".into(), serde_json::Value::from("有效数据"));
    row.insert("time_kind".into(), serde_json::Value::from("all_day"));
    row.insert("priority".into(), serde_json::Value::from(0));
    row.insert("is_completed".into(), serde_json::Value::from(0));
    row.insert("sort_order".into(), serde_json::Value::from(0));
    row.insert(
        "created_at".into(),
        serde_json::Value::from("2026-01-01T00:00:00+08:00"),
    );
    row.insert(
        "updated_at".into(),
        serde_json::Value::from("2026-01-01T00:00:00+08:00"),
    );
    row.insert("revision".into(), serde_json::Value::from(1));
    row.insert("未来版本才有的字段".into(), serde_json::Value::from(1));
    envelope
        .tables
        .insert("task".into(), vec![serde_json::Value::Object(row)]);

    let summary = import(&db, &envelope, true).expect("有效列应能正常导入");

    // 有效数据必须被写入
    assert_eq!(task_count(&db), 1, "有效列的数据应被导入");

    // 未知列必须被汇报出来
    let ignored = summary
        .ignored_columns
        .get("task")
        .unwrap_or_else(|| panic!("必须汇报被忽略的列，实际：{:?}", summary.ignored_columns));
    assert!(
        ignored.contains(&"未来版本才有的字段".to_string()),
        "被忽略的列名应出现在汇报里，实际：{ignored:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 注入式列名必须被**丢弃**，且不能影响同一行里合法列的数据。
#[test]
fn injection_column_is_dropped_without_breaking_valid_columns() {
    let (db, dir) = temp_db("drop-inject");

    let mut envelope: ExportEnvelope = export(&db).unwrap();
    let mut row = serde_json::Map::new();
    row.insert(
        "id".into(),
        serde_json::Value::from("dddddddd-0000-0000-0000-000000000004"),
    );
    row.insert("title".into(), serde_json::Value::from("合法标题"));
    row.insert("time_kind".into(), serde_json::Value::from("all_day"));
    row.insert("priority".into(), serde_json::Value::from(0));
    row.insert("is_completed".into(), serde_json::Value::from(0));
    row.insert("sort_order".into(), serde_json::Value::from(0));
    row.insert(
        "created_at".into(),
        serde_json::Value::from("2026-01-01T00:00:00+08:00"),
    );
    row.insert(
        "updated_at".into(),
        serde_json::Value::from("2026-01-01T00:00:00+08:00"),
    );
    row.insert("revision".into(), serde_json::Value::from(1));
    // 注入载荷作为列名
    row.insert(
        "x) VALUES (NULL); DELETE FROM task; --".into(),
        serde_json::Value::from(1),
    );
    envelope
        .tables
        .insert("task".into(), vec![serde_json::Value::Object(row)]);

    let summary = import(&db, &envelope, true).expect("合法列应能导入");

    // 注入被丢弃，合法数据完好
    assert_eq!(task_count(&db), 1, "注入载荷不应删除数据");
    assert_eq!(summary.total, 1);

    // task 表必须仍然存在
    let conn = db.lock().unwrap();
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='task'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(exists, 1, "task 表不该被注入载荷删除");

    let _ = std::fs::remove_dir_all(&dir);
}
