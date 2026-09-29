//! 领域模型与纯逻辑。
//!
//! 本层的代码不做 IO，因此可以在没有数据库、没有运行应用的情况下直接单元测试。
//! 重复规则推进、提醒档位计算、自然语言时间解析这些最容易出错的部分都会落在这里。

pub mod recurrence;
pub mod reminder;
pub mod task;
pub mod time;
