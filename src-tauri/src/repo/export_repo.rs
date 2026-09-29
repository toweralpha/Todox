//! 数据的导入与导出。
//!
//! # 设计决策：按行原样搬运，而不是反序列化成领域对象再重建
//!
//! 导入导出处理的是用户的**全部数据**，出错就是灾难性的丢失。因此这里刻意
//! 选择了一条更"笨"但更安全的路径：把数据库的行按原样读成 JSON，导入时再
//! 按原样写回。
//!
//! 相比"反序列化成 `Task` 结构体 → 校验 → 重新构造 INSERT"，这条路径的好处是：
//!
//! - **不丢字段**。新增一列时不需要同时改导入、导出、领域模型三处 —— 而漏改
//!   其中任何一处都会导致数据静默丢失，且只有用户真的去导入时才会发现。
//! - **保留原 ID**。重新构造通常会产生新 ID，那会让"导出再导入"变成数据重复
//!   而不是恢复，也无法与已完成记录、提醒档位关联。
//! - **保留时间戳与版本号**。用户的历史完成记录不该因为一次导出而"变成今天"。
//!
//! 代价是导入的数据绕过了 repository 层的校验。这个取舍是明确的：
//! 导出文件来自本应用自身，导入它属于"恢复"而非"接收外部输入"。

use serde::{Deserialize, Serialize};

use crate::db::connection::{Db, DbError};
use crate::domain::time::now_rfc3339;
use crate::repo::task_repo::RepoError;

/// 导出文件的格式版本。
///
/// 与数据库 schema 版本分开：文件格式的兼容性由导入方决定，
/// 两者演进的节奏不同（例如导出可以为了可读性重组字段而 schema 不变）。
pub const EXPORT_FORMAT_VERSION: u32 = 1;

/// 参与导出的表。
///
/// 顺序有讲究：父表必须在子表之前，导入时外键才不会瞬时失效。
/// `changelog` 刻意不导出 —— 它是纯本地的同步中间状态，
/// 把它搬到另一台机器上只会造成混乱。
const EXPORT_TABLES: &[&str] = &[
    "recurrence_rule",
    "task",
    "task_completion",
    "reminder",
    "missed_reminder",
    "tag",
    "task_tag",
    "subtask",
    "setting",
];

/// 导出文件的结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportEnvelope {
    /// 固定标识，用于确认"这确实是 Todox 导出的文件"。
    /// 没有它的话，用户误选一个任意 JSON 会得到难以理解的错误。
    pub app: String,
    pub format_version: u32,
    /// 导出时刻，仅供用户辨识文件新旧。
    pub exported_at: String,
    /// 各表的行，键为表名。
    ///
    /// 值用 `serde_json::Value` 而非具体结构体，正是为了"原样搬运" ——
    /// 表结构变化时这里不需要同步改动。
    pub tables: std::collections::BTreeMap<String, Vec<serde_json::Value>>,
}

impl ExportEnvelope {
    /// 统计总行数，用于向用户展示"导出了多少条数据"。
    pub fn total_rows(&self) -> usize {
        self.tables.values().map(|v| v.len()).sum()
    }
}

/// 把全部数据导出为一个信封结构。
pub fn export(db: &Db) -> Result<ExportEnvelope, RepoError> {
    let conn = db.lock()?;
    let mut tables = std::collections::BTreeMap::new();

    for table in EXPORT_TABLES {
        let mut stmt = conn.prepare(&format!("SELECT * FROM {table}"))?;
        let column_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();

        let mut rows_out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            // 逐列取值并转成 JSON。列类型由运行时决定，因此逐个尝试
            // 整数 → 浮点 → 文本 → NULL。
            let mut obj = serde_json::Map::new();
            for (i, name) in column_names.iter().enumerate() {
                let value = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => serde_json::Value::Null,
                    rusqlite::types::ValueRef::Integer(n) => serde_json::Value::from(n),
                    rusqlite::types::ValueRef::Real(f) => serde_json::Value::from(f),
                    rusqlite::types::ValueRef::Text(t) => {
                        serde_json::Value::from(String::from_utf8_lossy(t).to_string())
                    }
                    // BLOB 目前没有任何一列使用。若将来有用到，这里需要改成
                    // base64 之类的可移植表示 —— 直接塞字节数组会让 JSON 变得
                    // 巨大且难以人工检查。
                    rusqlite::types::ValueRef::Blob(_) => serde_json::Value::Null,
                };
                obj.insert(name.clone(), value);
            }
            rows_out.push(serde_json::Value::Object(obj));
        }

        tables.insert(table.to_string(), rows_out);
    }

    Ok(ExportEnvelope {
        app: "todox".into(),
        format_version: EXPORT_FORMAT_VERSION,
        exported_at: now_rfc3339(),
        tables,
    })
}

/// 导入的结果统计。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportSummary {
    /// 各表实际写入的行数。
    pub imported: std::collections::BTreeMap<String, usize>,
    pub total: usize,
    /// 被丢弃的未知列（表名 → 列名列表）。
    ///
    /// 未知列可能来自更新版本导出的备份。丢弃它们而不是让整个导入失败，
    /// 是为了"能恢复大部分数据"优先；但必须把丢弃了什么告诉用户，
    /// 否则就是静默的数据丢失。
    #[serde(default)]
    pub ignored_columns: std::collections::BTreeMap<String, Vec<String>>,
}

/// 把信封里的数据写入数据库。
///
/// `replace_existing` 为 true 时先清空现有数据（真正的"恢复"语义），
/// 为 false 时按主键合并（"合并另一台设备的数据"语义）。
///
/// # 为什么整个过程必须在一个事务里
///
/// 导入动辄写入上千行。若中途失败而部分写入生效，用户的数据库会处于
/// "一半是旧数据、一半是新数据"的状态，这比彻底失败糟糕得多 ——
/// 彻底失败至少数据还是完整的。因此任何一行出错都整体回滚。
pub fn import(
    db: &Db,
    envelope: &ExportEnvelope,
    replace_existing: bool,
) -> Result<ImportSummary, RepoError> {
    // 先校验文件来源与版本，再动数据库。
    // 顺序很重要：不能等清空了数据才发现文件不对。
    if envelope.app != "todox" {
        return Err(RepoError::Invalid(
            "这不是 Todox 导出的文件，请选择正确的 .json 备份".into(),
        ));
    }
    if envelope.format_version > EXPORT_FORMAT_VERSION {
        return Err(RepoError::Invalid(format!(
            "该备份来自更新版本的 Todox（格式 v{}，本程序支持到 v{}）。\
             请升级 Todox 后再导入。",
            envelope.format_version, EXPORT_FORMAT_VERSION
        )));
    }

    let mut conn = db.lock()?;
    let tx = conn.transaction()?;

    if replace_existing {
        // 逆序删除（子表先删），避免外键约束在删除父表时被触发
        for table in EXPORT_TABLES.iter().rev() {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        // changelog 不参与导入导出，但恢复语义下它记录的是"旧数据的变更"，
        // 留着会对将来的同步造成误导，因此一并清空。
        tx.execute("DELETE FROM changelog", [])?;
    }

    let mut imported = std::collections::BTreeMap::new();
    // 记录被丢弃的未知列，最后汇报给用户 —— 静默丢弃就是静默的数据丢失
    let mut ignored: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();

    for table in EXPORT_TABLES {
        let Some(rows) = envelope.tables.get(*table) else {
            // 旧版本导出的文件可能缺少后来新增的表 —— 跳过而不是报错，
            // 否则一个 v1 备份在 v2 程序里就永远无法导入了。
            imported.insert(table.to_string(), 0);
            continue;
        };

        let mut count = 0usize;
        for row in rows {
            let Some(obj) = row.as_object() else {
                continue;
            };
            if obj.is_empty() {
                continue;
            }

            let all_keys: Vec<&String> = obj.keys().collect();

            // **关键的安全步骤**：列名在 SQL 里是标识符，无法参数化。
            // 这里用实际表的列清单做白名单过滤，杜绝任何来自外部 JSON 的
            // 标识符被直接拼接进 SQL。
            let (columns, dropped) = filter_known_columns(&tx, table, &all_keys)?;
            if !dropped.is_empty() {
                let entry = ignored.entry(table.to_string()).or_default();
                for d in dropped {
                    if !entry.contains(&d) {
                        entry.push(d);
                    }
                }
            }

            if columns.is_empty() {
                // 整行都是未知列 —— 跳过它而不是拼一条没有列名的非法 SQL
                continue;
            }

            let placeholders: Vec<String> = (1..=columns.len()).map(|i| format!("?{i}")).collect();

            // 用 `ON CONFLICT ... DO UPDATE`（upsert）而**不是** `INSERT OR REPLACE`。
            //
            // 这不是风格偏好，而是正确性问题：SQLite 的 `INSERT OR REPLACE`
            // 在主键冲突时会**真的 DELETE 旧行再插入新行**，从而触发外键的
            // ON DELETE CASCADE —— 本地的完成记录、提醒档位、子任务、标签关联
            // 会被静默删掉；对 recurrence_rule 更会把引用它的 task.recurrence_id
            // 置为 NULL，留下"重复任务却没有规则"的坏状态。
            //
            // 而 upsert 走的是 UPDATE 路径，不触发任何外键动作，
            // 因此"合并"真正做到了"两条都保留（以导入的为准，但不牵连关联数据）"。
            //
            // 注意 UPDATE 子句里必须**排除主键列**：`DO UPDATE SET id = excluded.id`
            // 在语义上没有意义，且某些 SQLite 版本会因此报错。
            let update_clause: Vec<String> = columns
                .iter()
                .filter(|c| c.as_str() != "id")
                .map(|c| format!("{c} = excluded.{c}"))
                .collect();

            let sql = if update_clause.is_empty() {
                // 整行只有主键（理论上不该出现）：退化为"冲突就什么都不做"
                format!(
                    "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT DO NOTHING",
                    columns.join(", "),
                    placeholders.join(", ")
                )
            } else {
                format!(
                    "INSERT INTO {table} ({}) VALUES ({}) ON CONFLICT DO UPDATE SET {}",
                    columns.join(", "),
                    placeholders.join(", "),
                    update_clause.join(", ")
                )
            };

            let values: Vec<rusqlite::types::Value> =
                columns.iter().map(|c| json_to_sql(&obj[c])).collect();

            tx.execute(&sql, rusqlite::params_from_iter(values))?;
            count += 1;
        }

        imported.insert(table.to_string(), count);
    }

    tx.commit()?;

    let total = imported.values().sum();
    Ok(ImportSummary {
        imported,
        total,
        ignored_columns: ignored,
    })
}

/// 把信封里每行的列名过滤成该表**真实存在的列**。
///
/// # 为什么必须做这一步
///
/// 列名在 SQL 里处于**标识符位置**，不可能用参数绑定 —— 只能字符串拼接。
/// 而导入文件里的键名完全来自外部 JSON，是攻击者可控的。没有这道白名单，
/// 一个精心构造的列名就能闭合括号并追加任意 SQL 语句。
///
/// 这条路径的风险等级是**最高**的，因为它的入口是"用户打开一个 .json 文件"
/// 这样一个再普通不过的操作，而且导入前还会先清空数据（恢复模式）。
///
/// # 为什么是"过滤"而不是"报错"
///
/// 未知列可能合法地来自更新版本导出的文件（未来新增了字段）。
/// 因此选择**丢弃未知列并继续导入**，而不是让整个文件无法恢复 ——
/// 能恢复到大部分数据，比什么都恢复不了要好得多。
/// 被丢弃的列名会返回给调用方，由它向用户说明。
fn filter_known_columns(
    conn: &rusqlite::Connection,
    table: &str,
    keys: &[&String],
) -> Result<(Vec<String>, Vec<String>), RepoError> {
    let known = table_columns(conn, table)?;

    let mut keep = Vec::with_capacity(keys.len());
    let mut dropped = Vec::new();

    for k in keys {
        // 判断完全基于**实际表的列清单**，而不是任何形式的字符串清洗。
        // 白名单天然比黑名单安全：任何不在清单里的写法都进不去。
        if known.contains(k) {
            keep.push((*k).clone());
        } else {
            dropped.push((*k).clone());
        }
    }

    Ok((keep, dropped))
}

/// 读取某个表的列清单。
fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, RepoError> {
    // 表名：这里对 `table` 做一次白名单核对，确保它确实来自常量清单。
    // 虽然所有调用点传的都是 EXPORT_TABLES 里的值，但显式核对能防止
    // 将来有人从别处传入一个动态表名而无人察觉。
    if !EXPORT_TABLES.contains(&table) {
        return Err(RepoError::Invalid(format!(
            "内部错误：表名 {table} 不在允许导入的清单中"
        )));
    }

    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    let mut cols = Vec::new();
    while let Some(row) = rows.next()? {
        cols.push(row.get::<_, String>(1)?);
    }
    Ok(cols)
}

/// 把 JSON 值转成 SQLite 参数值。
fn json_to_sql(v: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value;
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Integer(i64::from(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                Value::Real(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        // 数组与对象在 schema 里没有对应列。序列化成 JSON 文本而不是丢弃，
        // 这样至少数据还在，将来若改用 JSON 列可以无损读取。
        other => Value::Text(other.to_string()),
    }
}

#[derive(Debug)]
pub enum ExportError {
    Db(DbError),
    Serialize(String),
    Parse(String),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::Db(e) => write!(f, "{e}"),
            ExportError::Serialize(m) => write!(f, "导出内容无法序列化：{m}"),
            ExportError::Parse(m) => write!(f, "文件内容无法解析：{m}"),
        }
    }
}

impl std::error::Error for ExportError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::task::NewTask;
    use crate::repo::task_repo::TaskRepo;

    fn seeded_db() -> Db {
        let db = Db::open_in_memory().expect("建库失败");
        let repo = TaskRepo::new(&db);

        repo.create(NewTask::inbox("任务甲")).unwrap();
        repo.create(NewTask::inbox("任务乙")).unwrap();

        let mut t = NewTask::inbox("带时间的任务");
        t.time_kind = crate::domain::task::TimeKind::AtTime;
        t.due_at = Some("2026-09-30T15:00:00+08:00".into());
        repo.create(t).unwrap();

        db
    }

    #[test]
    fn export_contains_all_tables() {
        let db = seeded_db();
        let env = export(&db).unwrap();

        assert_eq!(env.app, "todox");
        assert_eq!(env.format_version, EXPORT_FORMAT_VERSION);
        assert!(!env.exported_at.is_empty());

        for table in EXPORT_TABLES {
            assert!(env.tables.contains_key(*table), "导出结果应包含表 {table}");
        }
        assert_eq!(env.tables["task"].len(), 3);
    }

    /// changelog 是本地同步中间状态，导出它只会造成混乱。
    #[test]
    fn export_excludes_changelog() {
        let db = seeded_db();
        let env = export(&db).unwrap();
        assert!(
            !env.tables.contains_key("changelog"),
            "changelog 不应出现在导出文件中"
        );
    }

    /// 导出再导入到空库，数据应完整回来。
    #[test]
    fn export_then_import_roundtrip_preserves_data() {
        let src = seeded_db();
        let env = export(&src).unwrap();

        let dst = Db::open_in_memory().unwrap();
        let summary = import(&dst, &env, true).unwrap();

        assert_eq!(summary.imported["task"], 3);
        let tasks = TaskRepo::new(&dst).list_all(false).unwrap();
        assert_eq!(tasks.len(), 3);

        let titles: Vec<&str> = tasks.iter().map(|t| t.title.as_str()).collect();
        assert!(titles.contains(&"任务甲"));
        assert!(titles.contains(&"任务乙"));
        assert!(titles.contains(&"带时间的任务"));
    }

    /// **关键**：ID 必须被保留。若重新生成 ID，"导出再导入"会变成数据重复
    /// 而不是恢复，且完成记录与提醒档位会失去关联。
    #[test]
    fn import_preserves_ids() {
        let src = seeded_db();
        let before = TaskRepo::new(&src).list_all(false).unwrap();
        let env = export(&src).unwrap();

        let dst = Db::open_in_memory().unwrap();
        import(&dst, &env, true).unwrap();
        let after = TaskRepo::new(&dst).list_all(false).unwrap();

        let mut ids_before: Vec<&str> = before.iter().map(|t| t.id.as_str()).collect();
        let mut ids_after: Vec<&str> = after.iter().map(|t| t.id.as_str()).collect();
        ids_before.sort();
        ids_after.sort();
        assert_eq!(ids_before, ids_after, "导入后任务的 ID 必须完全一致");
    }

    /// 时间戳与版本号同样应原样保留。
    #[test]
    fn import_preserves_timestamps() {
        let src = seeded_db();
        let before = TaskRepo::new(&src).list_all(false).unwrap();
        let env = export(&src).unwrap();

        let dst = Db::open_in_memory().unwrap();
        import(&dst, &env, true).unwrap();
        let after = TaskRepo::new(&dst).list_all(false).unwrap();

        for b in &before {
            let a = after.iter().find(|t| t.id == b.id).expect("应有同名任务");
            assert_eq!(a.created_at, b.created_at, "created_at 不应被改写");
            assert_eq!(a.due_at, b.due_at, "due_at 不应被改写");
            assert_eq!(a.revision, b.revision, "revision 不应被改写");
        }
    }

    /// 合并模式（不清空）下，同 ID 的行应被覆盖而不是产生重复。
    #[test]
    fn import_merge_mode_upserts_by_id() {
        let src = seeded_db();
        let env = export(&src).unwrap();

        // 目标库先导入一次
        let dst = Db::open_in_memory().unwrap();
        import(&dst, &env, false).unwrap();
        assert_eq!(TaskRepo::new(&dst).list_all(false).unwrap().len(), 3);

        // 再导入一次（合并不清空）：不应变成 6 条
        import(&dst, &env, false).unwrap();
        assert_eq!(
            TaskRepo::new(&dst).list_all(false).unwrap().len(),
            3,
            "同 ID 的行应被覆盖，而不是重复插入"
        );
    }

    /// 恢复模式（清空）下，目标库原有的数据应被替换掉。
    #[test]
    fn import_replace_mode_clears_existing() {
        let src = seeded_db();
        let env = export(&src).unwrap();

        let dst = Db::open_in_memory().unwrap();
        TaskRepo::new(&dst)
            .create(NewTask::inbox("目标库原有的任务"))
            .unwrap();

        import(&dst, &env, true).unwrap();

        let titles: Vec<String> = TaskRepo::new(&dst)
            .list_all(false)
            .unwrap()
            .into_iter()
            .map(|t| t.title)
            .collect();
        assert_eq!(titles.len(), 3);
        assert!(
            !titles.contains(&"目标库原有的任务".to_string()),
            "恢复模式应清空原有数据"
        );
    }

    /// 拒绝非本应用导出的文件，并给出可理解的提示。
    #[test]
    fn rejects_foreign_file() {
        let db = Db::open_in_memory().unwrap();
        let mut env = export(&db).unwrap();
        env.app = "something-else".into();

        let err = import(&db, &env, true).expect_err("应拒绝外部文件");
        assert!(err.to_string().contains("Todox"), "提示应说明文件来源不对");
    }

    /// 拒绝来自更高版本的文件，而不是尝试导入后产生错乱数据。
    #[test]
    fn rejects_newer_format_version() {
        let db = Db::open_in_memory().unwrap();
        let mut env = export(&db).unwrap();
        env.format_version = EXPORT_FORMAT_VERSION + 1;

        let err = import(&db, &env, true).expect_err("应拒绝更新的格式");
        assert!(err.to_string().contains("升级"), "提示应建议升级程序");
    }

    /// 校验必须发生在清空数据之前 —— 否则文件不对也会先把用户数据删光。
    #[test]
    fn validation_happens_before_data_is_cleared() {
        let db = seeded_db();
        let mut env = export(&db).unwrap();
        env.app = "not-todox".into();

        let _ = import(&db, &env, true);

        assert_eq!(
            TaskRepo::new(&db).list_all(false).unwrap().len(),
            3,
            "文件校验失败时绝不能动原有数据"
        );
    }

    #[test]
    fn total_rows_counts_everything() {
        let db = seeded_db();
        let env = export(&db).unwrap();
        assert_eq!(env.total_rows(), 3, "3 个任务，暂无提醒与完成记录");
    }

    /// JSON 往返必须可行：导出能序列化，导入能反序列化。
    #[test]
    fn envelope_survives_json_roundtrip() {
        let db = seeded_db();
        let env = export(&db).unwrap();

        let text = serde_json::to_string_pretty(&env).unwrap();
        let parsed: ExportEnvelope = serde_json::from_str(&text).unwrap();

        let dst = Db::open_in_memory().unwrap();
        import(&dst, &parsed, true).unwrap();
        assert_eq!(TaskRepo::new(&dst).list_all(false).unwrap().len(), 3);
    }

    #[test]
    fn json_to_sql_maps_types() {
        use rusqlite::types::Value;
        assert_eq!(json_to_sql(&serde_json::Value::Null), Value::Null);
        assert_eq!(json_to_sql(&serde_json::json!(42)), Value::Integer(42));
        assert_eq!(
            json_to_sql(&serde_json::json!(true)),
            Value::Integer(1),
            "布尔值存整数，与 schema 的 CHECK 约束一致"
        );
        assert_eq!(
            json_to_sql(&serde_json::json!("文本")),
            Value::Text("文本".into())
        );
    }
}
