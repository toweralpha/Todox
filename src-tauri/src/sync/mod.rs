//! 云同步的抽象层。
//!
//! **当前阶段不实现任何真实的云端同步。** 本模块存在的唯一目的是让将来接入
//! 云端时不必改动数据层与 repository 层 —— 那些成本已经在表结构里预付了
//! （UUID 主键、软删除墓碑、`updated_at`、`revision`、`changelog` 表）。
//!
//! 具体方案见 README「未来接入云端同步的方案」。

use serde::{Deserialize, Serialize};

/// 变更操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    Insert,
    Update,
    Delete,
}

impl ChangeOp {
    pub fn as_db(self) -> &'static str {
        match self {
            ChangeOp::Insert => "insert",
            ChangeOp::Update => "update",
            ChangeOp::Delete => "delete",
        }
    }
}

/// 一条变更记录，对应 `changelog` 表的一行。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeRecord {
    /// 实体类型，如 `"task"`、`"tag"`。
    pub entity: String,
    pub entity_id: String,
    pub op: ChangeOp,
    /// 变更后的完整实体快照，序列化为 JSON。
    ///
    /// 存快照而非存字段差异（diff）：差异需要在接收端按顺序重放，
    /// 一旦中间某条丢失就会永久错位。快照是幂等的，重放与乱序都安全，
    /// 代价只是多占一点空间 —— 对本地优先的应用来说这是划算的交换。
    pub payload: serde_json::Value,
    pub revision: i64,
    pub changed_at: String,
}

/// 同步适配器。
///
/// 新增后端时只需实现本 trait，repository 层无需改动。
pub trait SyncAdapter {
    /// 把本地变更推送到远端。
    fn push(&self, changes: Vec<ChangeRecord>) -> Result<(), SyncError>;

    /// 拉取自 `since`（RFC3339 时间戳）之后远端发生的变更。
    fn pull(&self, since: &str) -> Result<Vec<ChangeRecord>, SyncError>;

    /// 解决同一记录在本地与远端都被修改的冲突。
    fn resolve_conflict(&self, local: ChangeRecord, remote: ChangeRecord) -> ChangeRecord;
}

/// 冲突解决策略：last-write-wins，并以 `revision` 作为同刻的判定依据。
///
/// 为什么不是字段级合并：字段级合并需要每个字段各带一个 `updated_at`，
/// 会让表结构与写入路径复杂化。而对一个单用户、多设备的待办应用来说，
/// 两条变更真正落在同一秒且互相冲突的情形极为罕见，LWW 的实际损失
/// 只是一个用户几乎不会注意到的字段覆盖。
///
/// 取舍写在 README 里，若将来真出现需要字段级合并的场景，改动范围
/// 只在本函数内。
pub fn last_write_wins(local: &ChangeRecord, remote: &ChangeRecord) -> ChangeRecord {
    match remote.changed_at.cmp(&local.changed_at) {
        std::cmp::Ordering::Greater => remote.clone(),
        std::cmp::Ordering::Less => local.clone(),
        // 时间戳相同（同一秒内的并发写入）时用 revision 决胜。
        // 再相同则保留本地：此时两者等价，取谁都不影响正确性，
        // 但固定取本地可以让行为可预测、可测试。
        std::cmp::Ordering::Equal => {
            if remote.revision > local.revision {
                remote.clone()
            } else {
                local.clone()
            }
        }
    }
}

/// 当前唯一实现：不联网，不做事。
///
/// 它的价值在于让上层代码从第一天起就走在"通过适配器访问同步"这条路径上，
/// 将来换成真实实现时调用方一行都不用改。
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalOnlyAdapter;

impl SyncAdapter for LocalOnlyAdapter {
    fn push(&self, _changes: Vec<ChangeRecord>) -> Result<(), SyncError> {
        // 没有远端要同步，直接丢弃。返回 Ok 而不是 Err：
        // 对调用方而言"本地优先模式下无需同步"是正常状态，不是故障。
        Ok(())
    }

    fn pull(&self, _since: &str) -> Result<Vec<ChangeRecord>, SyncError> {
        Ok(Vec::new())
    }

    fn resolve_conflict(&self, local: ChangeRecord, remote: ChangeRecord) -> ChangeRecord {
        last_write_wins(&local, &remote)
    }
}

#[derive(Debug)]
pub enum SyncError {
    /// 远端不可达。本地优先模式下不应因此影响任何本地操作。
    Unreachable(String),
    NotImplemented,
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Unreachable(m) => write!(f, "无法连接同步服务：{m}"),
            SyncError::NotImplemented => write!(f, "该同步后端尚未实现"),
        }
    }
}

impl std::error::Error for SyncError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(at: &str, revision: i64, op: ChangeOp) -> ChangeRecord {
        ChangeRecord {
            entity: "task".into(),
            entity_id: "11111111-1111-1111-1111-111111111111".into(),
            op,
            payload: serde_json::json!({}),
            revision,
            changed_at: at.into(),
        }
    }

    #[test]
    fn newer_remote_wins() {
        let local = rec("2026-09-29T10:00:00+08:00", 5, ChangeOp::Update);
        let remote = rec("2026-09-29T11:00:00+08:00", 5, ChangeOp::Update);
        assert_eq!(last_write_wins(&local, &remote), remote);
    }

    #[test]
    fn newer_local_wins() {
        let local = rec("2026-09-29T12:00:00+08:00", 5, ChangeOp::Update);
        let remote = rec("2026-09-29T11:00:00+08:00", 5, ChangeOp::Update);
        assert_eq!(last_write_wins(&local, &remote), local);
    }

    /// 同一秒内的并发写入靠 revision 决胜。
    #[test]
    fn equal_timestamp_uses_revision() {
        let local = rec("2026-09-29T10:00:00+08:00", 3, ChangeOp::Update);
        let remote = rec("2026-09-29T10:00:00+08:00", 7, ChangeOp::Update);
        assert_eq!(last_write_wins(&local, &remote), remote);
    }

    /// 完全相同时固定保留本地，保证行为可预测。
    #[test]
    fn full_tie_keeps_local() {
        let local = rec("2026-09-29T10:00:00+08:00", 3, ChangeOp::Update);
        let remote = rec("2026-09-29T10:00:00+08:00", 3, ChangeOp::Update);
        assert_eq!(last_write_wins(&local, &remote), local);
    }

    /// 本地优先模式下 push/pull 不应报错，否则会污染错误处理路径。
    #[test]
    fn local_only_adapter_is_a_noop_not_an_error() {
        let a = LocalOnlyAdapter;
        assert!(a.push(vec![]).is_ok());
        assert_eq!(a.pull("2026-01-01T00:00:00+08:00").unwrap().len(), 0);
    }
}
