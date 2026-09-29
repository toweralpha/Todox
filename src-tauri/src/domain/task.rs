//! 任务领域模型。
//!
//! 本模块是纯数据定义，不含任何 IO —— 测试不需要数据库，见文件末尾。
//!
//! 关于时间类型的核心约束：四种时间类型各自用独立字段承载，绝不合并成一个
//! 含义模糊的 `scheduled_at`。因为"倒计时该按哪个时间算"必须始终明确，
//! 如果需要从语义反推，渲染列表时就得靠猜。

use serde::{Deserialize, Serialize};

/// 时间类型。对应数据库中 `task.time_kind` 的取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TimeKind {
    /// 某时刻要做。使用 `due_at`。显示为「今天 14:30」，到点提醒。
    AtTime,
    /// 在某时刻之前完成。使用 `deadline_at`。显示为实时倒计时，分级提醒。
    BeforeDeadline,
    /// 全天任务，不限具体时刻。使用 `due_at`（仅日期部分有意义）。
    AllDay,
    /// 重复任务。使用 `recurrence_id` 指向规则，`due_at` 存首次发生时间。
    Recurring,
}

impl TimeKind {
    /// 转为数据库存储值。与 `schema.sql` 的 CHECK 约束必须一致。
    pub fn as_db(self) -> &'static str {
        match self {
            TimeKind::AtTime => "at_time",
            TimeKind::BeforeDeadline => "before_deadline",
            TimeKind::AllDay => "all_day",
            TimeKind::Recurring => "recurring",
        }
    }

    /// 从数据库值解析。
    ///
    /// 返回 `Option` 而非默认值：遇到无法识别的取值时应当报错，
    /// 静默降级成某种类型会让数据看起来正常但行为错误，比直接失败更难排查。
    pub fn from_db(s: &str) -> Option<Self> {
        match s {
            "at_time" => Some(TimeKind::AtTime),
            "before_deadline" => Some(TimeKind::BeforeDeadline),
            "all_day" => Some(TimeKind::AllDay),
            "recurring" => Some(TimeKind::Recurring),
            _ => None,
        }
    }

    /// 本类型用哪个字段作为时间基准。调度器与倒计时都依赖它。
    pub fn anchor_field(self) -> AnchorField {
        match self {
            TimeKind::BeforeDeadline => AnchorField::DeadlineAt,
            TimeKind::AtTime | TimeKind::AllDay | TimeKind::Recurring => AnchorField::DueAt,
        }
    }
}

/// 时间基准字段。存在的意义是让"按哪个时间算"成为类型层面的信息，
/// 而不是散落在各处 `if time_kind == ...` 的判断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnchorField {
    DueAt,
    DeadlineAt,
}

/// 优先级。0 表示无优先级，避免用 `Option` 增加无谓的嵌套。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub enum Priority {
    None = 0,
    Low = 1,
    Medium = 2,
    High = 3,
}

impl Priority {
    pub fn as_db(self) -> i64 {
        self as i64
    }

    pub fn from_db(v: i64) -> Option<Self> {
        match v {
            0 => Some(Priority::None),
            1 => Some(Priority::Low),
            2 => Some(Priority::Medium),
            3 => Some(Priority::High),
            _ => None,
        }
    }
}

impl TryFrom<i64> for Priority {
    type Error = String;

    fn try_from(v: i64) -> Result<Self, Self::Error> {
        Priority::from_db(v).ok_or_else(|| format!("无效的优先级取值：{v}"))
    }
}

impl From<Priority> for i64 {
    fn from(p: Priority) -> Self {
        p.as_db()
    }
}

/// 一条任务。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub note: Option<String>,
    pub time_kind: TimeKind,
    /// RFC3339 文本。`at_time` / `all_day` / `recurring` 使用。
    pub due_at: Option<String>,
    /// RFC3339 文本。仅 `before_deadline` 使用。
    pub deadline_at: Option<String>,
    pub recurrence_id: Option<String>,
    pub priority: Priority,
    pub is_completed: bool,
    pub completed_at: Option<String>,
    pub sort_order: i64,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub revision: i64,
}

/// 一次完成记录。
///
/// **为什么不复用 `Task.is_completed` 这个布尔字段**：重复任务需要每一轮的
/// 完成流水，才能在推进周期的同时保留"这个月做了几次锻炼"这类统计。
/// 布尔字段只能表达"当前是否完成"，无法承载历史。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskCompletion {
    pub id: String,
    pub task_id: String,
    /// 被完成的**那一次计划发生时刻**，区别于 `completed_at` 的实际完成时刻。
    /// 重复任务靠它判断"是哪一轮被完成了"。
    pub occurrence_at: Option<String>,
    pub completed_at: String,
    pub created_at: String,
    pub updated_at: String,
    pub deleted_at: Option<String>,
    pub revision: i64,
}

/// 编辑任务时的输入。
///
/// 所有字段都是 `Option`，其中 `None` 表示"不改这一项"。
///
/// 为什么不复用 [`NewTask`]（它的字段全是必填）：编辑界面通常只改一两项，
/// 用必填结构会迫使调用方把未改动的字段也回传一遍。那不仅啰嗦，更危险 ——
/// 若界面漏回传某个字段，它会被静默重置为默认值。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TaskEdit {
    pub title: Option<String>,
    pub note: Option<String>,
    pub priority: Option<Priority>,
    pub time_kind: Option<TimeKind>,
    /// 双层 `Option` 的外层表示"是否改动"，内层表示"改成什么"。
    ///
    /// 需要两层的理由：把任务的时间**删掉**（变成收件箱任务）是常见操作，
    /// 它必须能与"不动这个字段"区分开，单层 `Option` 无法表达这种差别。
    pub due_at: Option<Option<String>>,
    pub deadline_at: Option<Option<String>>,
}

/// 新建任务时的输入。
///
/// 与 [`Task`] 分开，是因为创建时不存在 `id`、`created_at` 这些由数据层
/// 生成的字段。让调用方去构造它们，等于把"谁来生成 ID"这个责任模糊化。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewTask {
    pub title: String,
    pub note: Option<String>,
    pub time_kind: TimeKind,
    pub due_at: Option<String>,
    pub deadline_at: Option<String>,
    pub recurrence_id: Option<String>,
    pub priority: Priority,
}

impl NewTask {
    /// 只带标题的任务，落到收件箱（无时间的普通待办）。
    ///
    /// `time_kind` 取 `AllDay` 且时间为空，语义上就是"没有安排时间"。
    pub fn inbox(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            note: None,
            time_kind: TimeKind::AllDay,
            due_at: None,
            deadline_at: None,
            recurrence_id: None,
            priority: Priority::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 确保 Rust 侧的类型名与 SQL 的 CHECK 约束逐字一致。
    ///
    /// 这个测试的价值在于：若有人改了 `as_db` 的返回值却没改 schema.sql，
    /// 插入任务时才会在运行时炸掉。这里提前发现。
    #[test]
    fn time_kind_db_values_match_schema() {
        for kind in [
            TimeKind::AtTime,
            TimeKind::BeforeDeadline,
            TimeKind::AllDay,
            TimeKind::Recurring,
        ] {
            assert_eq!(
                TimeKind::from_db(kind.as_db()),
                Some(kind),
                "{} 的数据库取值无法往返转换",
                kind.as_db()
            );
        }
    }

    #[test]
    fn time_kind_rejects_unknown_value() {
        assert_eq!(TimeKind::from_db("whenever"), None);
    }

    /// 倒计时基准的正确性直接决定"还剩多久"显示得对不对。
    #[test]
    fn anchor_field_matches_time_kind() {
        assert_eq!(
            TimeKind::BeforeDeadline.anchor_field(),
            AnchorField::DeadlineAt
        );
        assert_eq!(TimeKind::AtTime.anchor_field(), AnchorField::DueAt);
        assert_eq!(TimeKind::AllDay.anchor_field(), AnchorField::DueAt);
        assert_eq!(TimeKind::Recurring.anchor_field(), AnchorField::DueAt);
    }

    #[test]
    fn priority_roundtrips_and_rejects_out_of_range() {
        for p in [
            Priority::None,
            Priority::Low,
            Priority::Medium,
            Priority::High,
        ] {
            assert_eq!(Priority::from_db(p.as_db()), Some(p));
        }
        assert_eq!(Priority::from_db(4), None);
        assert_eq!(Priority::from_db(-1), None);
    }

    #[test]
    fn inbox_task_has_no_time() {
        let t = NewTask::inbox("买牛奶");
        assert!(t.due_at.is_none());
        assert!(t.deadline_at.is_none());
        assert_eq!(t.priority, Priority::None);
    }
}
