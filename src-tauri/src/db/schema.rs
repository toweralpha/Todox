//! 数据库 schema 版本管理。
//!
//! schema 的 DDL 放在同目录的 `schema.sql`，通过 `include_str!` 在编译期嵌入二进制。
//! 这样做的理由：运行时不需要在安装目录旁边附带一个 .sql 文件（尤其打包后
//! 安装目录可能是只读的），同时 SQL 又能被编辑器正确高亮和检查。

use rusqlite::Connection;

/// schema 版本号。
///
/// **每次修改 `schema.sql` 的破坏性结构时必须递增**，并同步在 `migrate` 中
/// 添加对应的迁移步骤。启动时会与数据库中的 `PRAGMA user_version` 比对。
///
/// - v1 = 阶段二初始 schema
/// - v2 = `reminder.offset_minutes` 改为 `offset_seconds`（支持秒级提醒）
pub const SCHEMA_VERSION: i64 = 2;

/// 首次建库的完整 DDL，编译期嵌入。
const SCHEMA_SQL: &str = include_str!("schema.sql");

/// 读取数据库当前的 schema 版本。
fn user_version(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
}

/// 某个表是否存在。
fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 某个表是否有某一列。
fn column_exists(conn: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 把数据库迁移到 [`SCHEMA_VERSION`]。
///
/// 迁移按版本逐级递进，每一步都必须能在**已有数据的库**上正确执行 ——
/// 用户的数据库里已经有任务、完成记录与提醒，任何一步出错都是数据损失。
pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let current = user_version(conn)?;

    if current > SCHEMA_VERSION {
        // 用户装过更新的版本又退回旧版本时会走到这里。
        // 此时不能继续用旧代码去写新结构，必须明确拒绝而不是悄悄损坏数据。
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!(
                "数据库 schema 版本为 {current}，高于本程序支持的 {SCHEMA_VERSION}。\
                 请升级 Todox，或改用其他数据库文件。"
            )),
        ));
    }

    // ---------- 全新数据库 ----------
    if current == 0 && !table_exists(conn, "task")? {
        // schema.sql 全部是 CREATE TABLE IF NOT EXISTS，因此执行一遍即可。
        conn.execute_batch(SCHEMA_SQL)?;
        conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
        return Ok(());
    }

    // ---------- 已有数据库：逐级迁移 ----------

    // v1 → v2：提醒偏移量的单位从分钟改为秒。
    //
    // 用 SQLite 原生的 RENAME COLUMN（3.25+）而不是"建新表→拷数据→删旧表"：
    // 后者会连带影响外键引用、索引与触发器，出错面大得多。
    if current < 2 {
        if column_exists(conn, "reminder", "offset_minutes")? {
            // 先加新列，再把旧值换算成秒填进去，最后删掉旧列。
            // 换算是 ×60 —— 这是这次迁移唯一需要"理解语义"的地方。
            conn.execute_batch(
                "ALTER TABLE reminder ADD COLUMN offset_seconds INTEGER NOT NULL DEFAULT 0;",
            )?;
            conn.execute_batch("UPDATE reminder SET offset_seconds = offset_minutes * 60;")?;
            conn.execute_batch("ALTER TABLE reminder DROP COLUMN offset_minutes;")?;
        }
        // 旧索引引用了被删掉的列，需要重建。
        // （schema.sql 里的 CREATE INDEX 是 IF NOT EXISTS，不会覆盖已有的旧索引。）
        conn.execute_batch("DROP INDEX IF EXISTS idx_reminder_task;")?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_reminder_task ON reminder(task_id) WHERE deleted_at IS NULL;",
        )?;
        conn.execute_batch("PRAGMA user_version = 2")?;
    }

    // 兜底：确保 schema.sql 里新增的表（如 missed_reminder）在任何版本上都被创建。
    // 只对已有库执行，全新库上面已经处理过了。
    conn.execute_batch(SCHEMA_SQL)?;

    // 最终把版本号对齐到当前值
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在一个内存库上跑迁移，验证 DDL 本身无语法错误、且可重复执行。
    ///
    /// 这个测试不需要任何外部文件或环境，因此可以作为每次改 schema 的第一道防线。
    #[test]
    fn migrate_creates_schema_and_is_idempotent() {
        let conn = Connection::open_in_memory().expect("打开内存库失败");

        migrate(&conn).expect("首次迁移应当成功");
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);

        // 再跑一次不应报错（模拟"已是最新版本时重复启动"）
        migrate(&conn).expect("重复迁移应当幂等成功");

        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        for expected in [
            "task",
            "recurrence_rule",
            "task_completion",
            "reminder",
            "missed_reminder",
            "tag",
            "task_tag",
            "subtask",
            "setting",
            "changelog",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "缺少表 {expected}，实际存在：{tables:?}"
            );
        }
    }

    /// 数据库版本高于程序时必须拒绝启动，而不是继续写入导致结构错乱。
    #[test]
    fn migrate_rejects_newer_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA user_version = 999").unwrap();

        let err = migrate(&conn).expect_err("应当拒绝更新的 schema");
        assert!(
            err.to_string().contains("999"),
            "错误信息应包含实际版本号，实际为：{err}"
        );
    }

    /// v1 → v2 迁移：提醒偏移量的单位必须从分钟正确换算成秒。
    ///
    /// 这是本项目第一次真正的数据迁移，也是唯一一处需要"理解语义"的改动 ——
    /// 换算写错会让用户所有提醒的时间整体偏移 60 倍，而且不会报任何错。
    #[test]
    fn migration_v1_to_v2_converts_offset_unit() {
        let conn = Connection::open_in_memory().unwrap();

        // 手工构造一个 v1 结构：offset_minutes 列 + 若干条已有数据
        conn.execute_batch(
            "CREATE TABLE task (
                id TEXT PRIMARY KEY NOT NULL, title TEXT NOT NULL, note TEXT,
                time_kind TEXT NOT NULL, due_at TEXT, deadline_at TEXT, recurrence_id TEXT,
                priority INTEGER NOT NULL DEFAULT 0, is_completed INTEGER NOT NULL DEFAULT 0,
                completed_at TEXT, sort_order INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT,
                revision INTEGER NOT NULL DEFAULT 1
             );
             CREATE TABLE reminder (
                id TEXT PRIMARY KEY NOT NULL,
                task_id TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
                offset_minutes INTEGER NOT NULL,
                snooze_until TEXT,
                is_enabled INTEGER NOT NULL DEFAULT 1,
                last_fired_at TEXT,
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL, deleted_at TEXT,
                revision INTEGER NOT NULL DEFAULT 1
             );
             CREATE INDEX idx_reminder_task ON reminder(task_id) WHERE deleted_at IS NULL;
             INSERT INTO task (id,title,time_kind,priority,is_completed,sort_order,
                               created_at,updated_at,revision)
             VALUES ('t1','测试任务','before_deadline',0,0,0,'2026-01-01T00:00:00+08:00',
                     '2026-01-01T00:00:00+08:00',1);
             INSERT INTO reminder (id,task_id,offset_minutes,is_enabled,created_at,updated_at,revision)
             VALUES ('r1','t1',-1440,1,'2026-01-01T00:00:00+08:00','2026-01-01T00:00:00+08:00',1);
             INSERT INTO reminder (id,task_id,offset_minutes,is_enabled,created_at,updated_at,revision)
             VALUES ('r2','t1',-60,1,'2026-01-01T00:00:00+08:00','2026-01-01T00:00:00+08:00',1);
             INSERT INTO reminder (id,task_id,offset_minutes,is_enabled,created_at,updated_at,revision)
             VALUES ('r3','t1',0,1,'2026-01-01T00:00:00+08:00','2026-01-01T00:00:00+08:00',1);
             PRAGMA user_version = 1;",
        )
        .unwrap();

        migrate(&conn).expect("v1 → v2 迁移应当成功");
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);

        // 旧列应已消失，新列应存在
        assert!(
            !column_exists(&conn, "reminder", "offset_minutes").unwrap(),
            "旧的 offset_minutes 列应已被删除"
        );
        assert!(
            column_exists(&conn, "reminder", "offset_seconds").unwrap(),
            "应存在 offset_seconds 列"
        );

        // 数值必须正确换算为秒：-1440 分 → -86400 秒，-60 分 → -3600 秒
        let offsets: Vec<i64> = conn
            .prepare("SELECT offset_seconds FROM reminder ORDER BY offset_seconds")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            offsets,
            vec![-86400, -3600, 0],
            "偏移量的单位换算必须是 ×60"
        );

        // 任务数据必须完好
        let title: String = conn
            .query_row("SELECT title FROM task WHERE id='t1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "测试任务", "迁移不应影响任务数据");

        // 新表应被创建
        assert!(
            table_exists(&conn, "missed_reminder").unwrap(),
            "迁移后应存在 missed_reminder 表"
        );
    }

    /// 迁移必须幂等：重复启动不应反复换算（那会让偏移量每次 ×60）。
    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        // 注意这里分三条语句执行：rusqlite 的 `query_row` / `execute` 只处理
        // **单条** SQL，把 INSERT 与 SELECT 写进同一个字符串会静默地只执行第一条，
        // 于是 SELECT 拿不到任何行 —— 这类错误在测试里表现为难以理解的 panic。
        conn.execute(
            "INSERT INTO task (id,title,time_kind,priority,is_completed,sort_order,
                               created_at,updated_at,revision)
             VALUES ('t','x','all_day',0,0,0,'2026-01-01T00:00:00+08:00',
                     '2026-01-01T00:00:00+08:00',1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO reminder (id,task_id,offset_seconds,is_enabled,
                                   created_at,updated_at,revision)
             VALUES ('r','t',-3600,1,'2026-01-01T00:00:00+08:00',
                     '2026-01-01T00:00:00+08:00',1)",
            [],
        )
        .unwrap();

        let before: i64 = conn
            .query_row(
                "SELECT offset_seconds FROM reminder WHERE id='r'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, -3600);

        // 再跑几次迁移
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();

        let after: i64 = conn
            .query_row(
                "SELECT offset_seconds FROM reminder WHERE id='r'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after, before,
            "重复迁移绝不能再次换算 —— 那会让偏移量变成 -3600×60"
        );
    }
}
