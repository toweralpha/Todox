//! 数据库连接的建立与运行期参数。
//!
//! 本模块只负责"得到一个配置正确的连接"，不含任何业务逻辑。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::Connection;

use crate::db::schema;

/// 应用状态中持有的数据库句柄。
///
/// # 为什么是 `Arc<Mutex<Connection>>`
///
/// - `Mutex` 而非 `RefCell`：`Mutex<Connection>` 是 `Send + Sync` 的，
///   因此这个类型可以被调度器所在的后台线程使用。阶段四的调度器需要在
///   独立线程上按时刻读取任务来推导触发点，若图省事用 `RefCell`，
///   届时必须回头改造整个数据层。
/// - `Arc` 是为了**可克隆**：数据库句柄同时被应用状态（供命令访问）与
///   调度器（供后台循环访问）持有。共享同一个连接而非各开一个，是为了让
///   两者看到一致的数据并能共用同一个写锁 —— 独立连接会引入 SQLite 的
///   锁竞争，且 WAL 下的可见性判断更复杂。
#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    /// 打开（或创建）位于 `dir` 下的数据库，并完成迁移。
    pub fn open(dir: &Path) -> Result<Self, DbError> {
        std::fs::create_dir_all(dir).map_err(|e| DbError::Io {
            path: dir.to_path_buf(),
            source: e,
        })?;

        let path = dir.join("todox.db");
        let conn = Connection::open(&path).map_err(DbError::Sqlite)?;

        configure(&conn)?;
        schema::migrate(&conn)?;

        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// 在内存中建库。仅供测试使用 —— 进程退出即消失，永不落盘。
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory().map_err(DbError::Sqlite)?;
        configure(&conn)?;
        schema::migrate(&conn)?;
        Ok(Self(Arc::new(Mutex::new(conn))))
    }

    /// 取得连接锁。
    ///
    /// 关于锁中毒：只有在持锁期间发生 panic 才会中毒。此时内存中的
    /// `Connection` 可能处于事务中途的不确定状态，继续使用它有可能写入
    /// 半成品数据。因此这里选择把中毒当作致命错误向上传播，让应用带着
    /// 明确信息失败，而不是 `unwrap()` 掉一个可能损坏的数据库句柄。
    pub fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, DbError> {
        self.0.lock().map_err(|_| DbError::Poisoned)
    }
}

/// 设置运行期参数。
///
/// 这些 PRAGMA 大多作用于**当前连接**而非数据库文件本身，因此每次建立连接
/// 都必须重新设置，不能只在首次建库时执行一次。
fn configure(conn: &Connection) -> Result<(), DbError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;
         PRAGMA temp_store = MEMORY;",
    )?;

    // 这些是纯性能调优项，允许失败：例如某些 SQLite 构建未启用相关编译选项。
    // 它们不影响正确性，因此失败时不应阻断启动。
    let _ = conn.execute_batch(
        "PRAGMA cache_size = -8000;
         PRAGMA mmap_size = 67108864;",
    );

    Ok(())
}

/// 数据库层的错误类型。
///
/// 单独定义而不直接用 `rusqlite::Error`，是为了把"文件系统问题"和
/// "锁中毒"这两类非 SQL 故障也纳入同一个错误类型，便于上层统一处理。
#[derive(Debug)]
pub enum DbError {
    Sqlite(rusqlite::Error),
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 连接锁中毒，见 [`Db::lock`] 的说明。
    Poisoned,
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Sqlite(e) => write!(f, "数据库错误：{e}"),
            DbError::Io { path, source } => {
                write!(f, "无法访问 {}：{source}", path.display())
            }
            DbError::Poisoned => write!(f, "数据库连接状态异常，请重启 Todox"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DbError::Sqlite(e) => Some(e),
            DbError::Io { source, .. } => Some(source),
            DbError::Poisoned => None,
        }
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(e: rusqlite::Error) -> Self {
        DbError::Sqlite(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_in_memory_applies_pragmas_and_schema() {
        let db = Db::open_in_memory().expect("建库失败");
        let conn = db.lock().unwrap();

        // 内存库不支持 WAL（会返回 "memory"），因此这里验证的是
        // 那些决定正确性的参数确实生效了。
        let fk: i64 = conn
            .query_row("PRAGMA foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(fk, 1, "外键约束必须开启，否则级联删除不会生效");

        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);
    }

    #[test]
    fn open_creates_missing_directory() {
        let base = std::env::temp_dir().join(format!("todox-test-{}", std::process::id()));
        let nested = base.join("a").join("b");
        // 确保起点干净
        let _ = std::fs::remove_dir_all(&base);

        let db = Db::open(&nested).expect("应当自动创建多级目录");
        drop(db);
        assert!(nested.join("todox.db").exists(), "数据库文件未生成");

        let _ = std::fs::remove_dir_all(&base);
    }
}
