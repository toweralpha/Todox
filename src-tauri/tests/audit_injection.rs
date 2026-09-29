//! 验证 `import` 对来自外部 JSON 的**列名**是否做了防护。
//!
//! 背景：列名在 SQL 里处于**标识符位置**，不可能用参数绑定 —— 只能字符串拼接。
//! 而导入文件里的键名完全来自外部 JSON，是攻击者可控的。
//! 这条路径的入口是"用户打开一个 .json 文件"这样一个再普通不过的操作，
//! 且恢复模式还会先清空数据，因此风险等级最高。
//!
//! 这些测试的目标是**尝试真正注入**，而不是复述设计意图。

use std::collections::BTreeMap;

use todox_lib::db::connection::Db;
use todox_lib::domain::task::{NewTask, TimeKind};
use todox_lib::repo::export_repo::{export, import, ExportEnvelope};
use todox_lib::repo::task_repo::TaskRepo;

/// 建一个临时目录下的真实数据库（open_in_memory 只在 crate 内部测试可用）。
fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "todox-audit-{}-{}-{}",
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

fn task_count(db: &Db) -> i64 {
    TaskRepo::new(db)
        .table_counts()
        .unwrap()
        .into_iter()
        .find(|(t, _)| t == "task")
        .map(|(_, c)| c)
        .unwrap()
}

fn table_exists(db: &Db, name: &str) -> bool {
    let conn = db.lock().unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [name],
            |r| r.get(0),
        )
        .unwrap();
    n > 0
}

/// 未知列必须被忽略并**汇报**，且绝不能进入 SQL。
#[test]
fn unknown_column_is_ignored_and_reported() {
    let (db, dir) = temp_db("unknown-col");

    let mut t = NewTask::inbox("重要数据");
    t.time_kind = TimeKind::AllDay;
    TaskRepo::new(&db).create(t).unwrap();

    let mut envelope: ExportEnvelope = export(&db).unwrap();
    let mut evil_row = serde_json::Map::new();
    evil_row.insert("不存在的列名".to_string(), serde_json::Value::from("x"));
    envelope
        .tables
        .insert("task".into(), vec![serde_json::Value::Object(evil_row)]);

    let summary = import(&db, &envelope, false).expect("过滤后应能继续导入");

    let ignored = summary
        .ignored_columns
        .get("task")
        .expect("必须汇报被忽略的列，否则就是静默的数据丢失");
    assert!(
        ignored.contains(&"不存在的列名".to_string()),
        "被忽略的列名应出现在汇报里，实际：{ignored:?}"
    );
    assert_eq!(
        summary.imported.get("task").copied().unwrap_or(0),
        0,
        "整行都是非法列时应跳过该行，不写入任何内容"
    );
    assert_eq!(task_count(&db), 1, "原有数据必须完好");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 注入式列名必须被丢弃、被汇报，且**绝不能被执行**。
#[test]
fn sql_injection_style_column_names_are_neutralized() {
    let payloads = [
        "x) VALUES (NULL); DELETE FROM task; --",
        "(SELECT 1) --",
        "id\" , (SELECT 1) --",
        "id; DROP TABLE task; --",
        "`id`",
        "id) VALUES (1); DROP TABLE setting; --",
    ];

    for p in payloads {
        let (db, dir) = temp_db("inj-payload");

        let mut t = NewTask::inbox("不可被删");
        t.time_kind = TimeKind::AllDay;
        TaskRepo::new(&db).create(t).unwrap();

        let mut envelope: ExportEnvelope = export(&db).unwrap();
        let mut row = serde_json::Map::new();
        row.insert(p.to_string(), serde_json::Value::from(1));
        envelope
            .tables
            .insert("task".into(), vec![serde_json::Value::Object(row)]);

        let summary = import(&db, &envelope, false).expect("应被安全过滤后继续");

        assert!(
            summary
                .ignored_columns
                .get("task")
                .map(|v| v.contains(&p.to_string()))
                .unwrap_or(false),
            "注入载荷 {p:?} 必须被识别为未知列并汇报"
        );
        assert_eq!(task_count(&db), 1, "注入载荷 {p:?} 破坏了用户数据");

        for table in ["task", "setting", "reminder"] {
            assert!(table_exists(&db, table), "载荷 {p:?} 删除了 {table} 表");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// 表名来自白名单常量，恶意表名应被忽略而不是执行。
#[test]
fn table_names_are_whitelisted() {
    let (db, dir) = temp_db("table-whitelist");
    let mut envelope: ExportEnvelope = export(&db).unwrap();

    let mut tables: BTreeMap<String, Vec<serde_json::Value>> = BTreeMap::new();
    tables.insert(
        "task; DROP TABLE setting; --".to_string(),
        vec![serde_json::Value::Object(serde_json::Map::new())],
    );
    envelope.tables = tables;

    let result = import(&db, &envelope, false);
    assert!(result.is_ok(), "未知表名应被忽略而不是报错：{result:?}");
    assert!(
        table_exists(&db, "setting"),
        "setting 表必须未被删除（表名有白名单保护）"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 空对象行、非对象行、超长列名都不应导致 panic 或写入脏数据。
#[test]
fn malformed_rows_are_handled_gracefully() {
    let (db, dir) = temp_db("malformed");
    let mut envelope: ExportEnvelope = export(&db).unwrap();

    envelope.tables.insert(
        "task".into(),
        vec![
            // 空对象
            serde_json::Value::Object(serde_json::Map::new()),
            // 根本不是对象
            serde_json::Value::from("这不是对象"),
            serde_json::Value::from(42),
            serde_json::Value::Null,
            // 超长列名
            {
                let mut m = serde_json::Map::new();
                m.insert("x".repeat(100_000), serde_json::Value::from(1));
                serde_json::Value::Object(m)
            },
        ],
    );

    // 不应 panic
    let result = import(&db, &envelope, false);
    assert!(result.is_ok(), "畸形行应被跳过而不是让导入崩溃：{result:?}");
    assert_eq!(task_count(&db), 0, "畸形行不应写入任何数据");

    let _ = std::fs::remove_dir_all(&dir);
}
