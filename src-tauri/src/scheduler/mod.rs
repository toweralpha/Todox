//! 提醒调度器。
//!
//! # 核心设计：睡到下一个触发时刻，绝不轮询
//!
//! 本模块**没有任何周期性检查**（没有 `setInterval` 式的循环）。整个循环只做
//! 一件事：从数据库推导出全部未来的提醒触发时刻，取最早的一个，`sleep_until`
//! 睡到那一刻再醒来。因此空闲时 CPU 占用为 0，而不是"很低"。
//!
//! 这对一个 7×24 挂着、其中 99% 时间无事可做的进程来说是必须的：
//! 任何"每秒检查一次"的设计都会让 CPU 永远无法归零。
//!
//! # 为什么不物化"下次触发时刻"
//!
//! 数据库里不保存"下一次通知该在何时弹"这类字段。每次需要时都从 `task` 与
//! `reminder` 重新推导。理由：进程被杀、系统休眠、用户改系统时钟之后，任何
//! 物化的调度状态都必然与现实脱节，且无法判断它是"过期了"还是"还没到"。
//! 重推导是幂等的 —— 无论何时重启，结果都一致。
//!
//! # 唤醒通道
//!
//! 只靠 sleep 是不够的：用户在应用运行期间新建了一个 5 分钟后的任务，
//! 而调度器可能正睡在 3 小时后。因此外部改动通过 `watch` 通道通知调度器
//! "重算并重睡"。这是事件驱动，不是轮询。

use std::time::Duration;

use chrono::{DateTime, FixedOffset, Local, NaiveDateTime, TimeZone};
use tokio::sync::watch;

use crate::db::connection::Db;
use crate::domain::reminder::{all_day_base, fire_times, is_missed, ReminderTier};
use crate::domain::task::{AnchorField, TimeKind};
use crate::repo::reminder_repo::ReminderRepo;
use crate::repo::settings_repo::SettingsRepo;

/// 调度器需要弹出的一个提醒。
#[derive(Debug, Clone, PartialEq)]
pub struct FiringReminder {
    pub task_id: String,
    pub task_title: String,
    /// 本应触发的时刻。
    pub scheduled_at: DateTime<FixedOffset>,
    /// 该提醒档位的偏移（分钟）。用于「稍后提醒」时回写。
    pub offset_seconds: i64,
    /// 是否属于"已经错过"的补发。
    ///
    /// 单独标记是为了让通知文案能区分"现在到点了"与"你错过了"——
    /// 两者对用户的意义完全不同，混为一谈会让人以为自己错过了什么。
    pub is_missed: bool,
    /// 这个档位最近一次实际弹出的时刻。
    ///
    /// 补发逻辑必须看它，否则会重复处理已经正常触发过的提醒 ——
    /// 表现是"你错过了 3 个提醒"里混进一个其实准时弹过的，
    /// 用户会以为自己漏了什么。
    pub last_fired_at: Option<DateTime<FixedOffset>>,
    /// 待处理的"稍后提醒"时刻。存在时它取代档位偏移作为触发时刻。
    pub snooze_until: Option<DateTime<FixedOffset>>,
}

impl FiringReminder {
    /// 判断这一条是否已经处理过了。
    ///
    /// 按**时间戳**比较而非按日期：`last_fired_at` 写的是真实墙钟时间，
    /// 而 `scheduled_at` 可能落在未来（"稍后提醒"把它推后了）。若按日期比较，
    /// 一个昨天触发、计划时刻在明天的档位会被误判为"尚未处理"。
    ///
    /// 判据是"最近一次触发是否不早于本次计划时刻" —— 是则说明本次已经弹过。
    fn already_handled(&self) -> bool {
        match self.last_fired_at {
            Some(fired) => fired.timestamp() >= self.scheduled_at.timestamp(),
            None => false,
        }
    }
}

/// 调度器的可共享句柄。
///
/// 内部只有一个 `watch::Sender`：外部改动时调用 [`Self::wake`] 通知调度器重算。
/// `watch` 而非 `mpsc` 是因为我们不需要排队 —— 连着改十次任务，调度器只要
/// 醒来重算一次就够了，多余的唤醒信号没有意义。
#[derive(Clone)]
pub struct SchedulerHandle {
    wake_tx: watch::Sender<u64>,
}

impl SchedulerHandle {
    /// 通知调度器"数据变了，重算下次触发时刻"。
    ///
    /// 任何会改变提醒集合的操作之后都必须调用它（新建/修改/删除任务、
    /// 改动设置、完成一轮重复任务）。漏调的后果是提醒延迟到下一次
    /// 数据变动才生效 —— 因此调用点都紧贴着 repository 写入。
    pub fn wake(&self) {
        // send_modify 原地递增，不需要接收方立即读取。
        // 刻意不给返回值绑定变量：`send_modify` 返回 `()`，
        // 写成 `let _ = ...` 只会让"这里有个 Result 要处理"的错觉产生。
        // 没有接收方说明调度器尚未启动或已结束，那属于正常状态
        // （例如集成测试里只测数据层）。
        self.wake_tx.send_modify(|v| *v = v.wrapping_add(1));
    }
}

/// 启动调度器。
///
/// 返回的句柄必须在会改变提醒的操作后调用 `wake()`。
pub fn spawn(db: Db) -> SchedulerHandle {
    let (wake_tx, wake_rx) = watch::channel(0u64);
    let handle = SchedulerHandle {
        wake_tx: wake_tx.clone(),
    };

    // 优先取 AppHandle、其次窗口。两者都没有时通知只写日志 ——
    // 数据层的正确性（提醒是否被记录为已触发）不依赖界面是否存在。
    tauri::async_runtime::spawn(async move {
        run_loop(db, wake_rx).await;
    });

    handle
}

/// 调度主循环。
async fn run_loop(db: Db, mut wake_rx: watch::Receiver<u64>) {
    loop {
        // **每一轮都补发错过的提醒**，而不只在启动时补一次。
        //
        // 早期实现只在启动时调用 `catch_up_missed`，导致三类真实漏弹：
        //
        //   1. `select!` 竞态：定时器到点与 `wake()` 同时就绪时，tokio 会
        //      **随机**选一支。若选中 wake 分支，本次触发就被丢弃；下一轮
        //      `now` 已晚于 `scheduled_at`，被"只取严格未来"的条件过滤掉 ——
        //      既不弹通知也不记 missed，永久消失。
        //   2. `notifier::show` 是同步阻塞调用（PowerShell 冷启动数百毫秒），
        //      期间到期的另一条提醒会在下一轮被当成"过去"丢弃。
        //   3. 系统睡眠/休眠/改钟后，睡眠期间到期的提醒全部落进"过去"。
        //
        // 把补发放进循环里，这三种情况都被同一个机制兜住。
        // 该函数是幂等的（`record_missed` 按 (task_id, scheduled_at) 去重），
        // 因此重复调用不会产生重复记录。
        catch_up_missed(&db);

        // 补发之后再取时间：上面可能写了库、也可能耗时，
        // 用一个更晚的 `now` 才能算出正确的睡眠时长。
        // 统一用 `FixedOffset`：`Local` 与 `FixedOffset` 在 chrono 里是
        // 不同的类型参数，混用会无法做减法比较。
        let now = Local::now().fixed_offset();

        // 推导下一次触发时刻。None 表示当前没有任何待触发的提醒。
        let next = match next_firing(&db, now) {
            Ok(n) => n,
            Err(e) => {
                // 数据库读失败（例如文件被占用）。不能 panic —— 那会让整个
                // 常驻进程退出。退避 30 秒后重试即可。
                eprintln!("调度器读取提醒失败：{e}");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };

        match next {
            Some(firing) => {
                // 计算还需睡多久。用 chrono 求差而非时间戳相减，
                // 以正确处理跨时区偏移的情形。
                let delta = firing.scheduled_at.signed_duration_since(now);
                let sleep_for = delta
                    .to_std()
                    // 已经过期（小于 0）：不睡，立即处理
                    .unwrap_or(Duration::from_millis(0));

                // 这里是整个模块的核心：要么睡到那一刻，要么被唤醒信号打断。
                // 两者之外不会有任何周期性的醒来。
                //
                // `biased` 让分支按书写顺序求值：**定时器优先于唤醒信号**。
                // 没有它时，两分支同时就绪会让 tokio 随机选择，
                // 于是"恰好到点时来了一个 wake"就可能把这次触发丢掉。
                tokio::select! {
                    biased;

                    _ = tokio::time::sleep(sleep_for) => {
                        if let Err(e) = fire(&db, &firing) {
                            eprintln!("触发提醒失败：{e}");
                        }
                    }
                    // 数据变了 → 跳出 select 重新推导，不在这里处理具体变更。
                    // 本轮已到点的触发不会丢：下一轮开头的 catch_up_missed
                    // 会把它作为"错过"补上。
                    res = wake_rx.changed() => {
                        if res.is_err() {
                            // 发送端全部丢弃，说明应用正在退出
                            break;
                        }
                    }
                }
            }
            None => {
                // 没有任何待触发的提醒：无限期等待唤醒信号。
                //
                // 注意这里**不是**轮询 —— 没有 timeout，进程真正进入睡眠，
                // CPU 占用为零。新建任务时会通过 wake() 把它叫醒。
                if wake_rx.changed().await.is_err() {
                    break;
                }
            }
        }
    }
}

/// 推导出下一个应当触发的提醒（严格在未来）。
///
/// 返回 `None` 表示当前没有任何未来的提醒。
fn next_firing(db: &Db, now: DateTime<FixedOffset>) -> Result<Option<FiringReminder>, String> {
    let candidates = upcoming_reminders(db, now)?;

    // 取严格大于 now 的最早一个。
    // 过期的由 catch_up_missed 负责，不在这里处理 —— 混在一起会导致
    // 启动补发与正常触发重复弹出同一条提醒。
    Ok(candidates
        .into_iter()
        .filter(|r| r.scheduled_at > now)
        .min_by_key(|r| r.scheduled_at))
}

/// 列出全部待触发的提醒（含已过期的）。
/// 推导出全部待触发的提醒（含已过期的），按时间升序。
///
/// 公开给集成测试：验证"用户输入 → 真的安排出提醒"这条链路时，
/// 需要看到调度器**实际**推导出的触发点，而不只是数据库里的时间字段。
/// 若时间字段对了、调度器却推导不出触发点，用户依然收不到提醒 ——
/// 而那正是"输入了时间却毫无反馈"这类问题的隐藏形态。
pub fn upcoming_reminders(
    db: &Db,
    now: DateTime<FixedOffset>,
) -> Result<Vec<FiringReminder>, String> {
    let settings = SettingsRepo::new(db).load().map_err(|e| e.to_string())?;
    let repo = ReminderRepo::new(db);

    let tasks = repo.tasks_with_reminders().map_err(|e| e.to_string())?;

    let mut out = Vec::new();

    for (task, tiers) in tasks {
        // 取任务的时间基准
        let anchor_str = match task.time_kind.anchor_field() {
            AnchorField::DueAt => task.due_at.as_deref(),
            AnchorField::DeadlineAt => task.deadline_at.as_deref(),
        };
        let Some(anchor_str) = anchor_str else {
            continue;
        };
        let Ok(anchor) = DateTime::parse_from_rfc3339(anchor_str) else {
            continue;
        };

        // 全天任务的基准是当天 00:00，直接用它做提醒基准会导致半夜弹通知。
        // 这里把它挪到用户设定的钟点（默认 09:00）。
        let base = if task.time_kind == TimeKind::AllDay {
            match all_day_base(
                anchor.date_naive(),
                settings.all_day_hour,
                settings.all_day_minute,
                *anchor.offset(),
            ) {
                Some(b) => b,
                None => continue,
            }
        } else {
            anchor
        };

        let enabled: Vec<ReminderTier> = tiers
            .iter()
            .filter(|t| t.is_enabled)
            .map(|t| ReminderTier {
                offset_seconds: t.offset_seconds,
                is_enabled: true,
            })
            .collect();

        for (mut scheduled_at, tier) in fire_times(&enabled, base) {
            // 找到这个档位对应的原始行，取出 last_fired_at 与 snooze_until
            let row = tiers
                .iter()
                .find(|t| t.offset_seconds == tier.offset_seconds);
            let last_fired_at = row
                .and_then(|r| r.last_fired_at.as_deref())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok());
            let snooze_until = row
                .and_then(|r| r.snooze_until.as_deref())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok());

            // 「稍后提醒」一旦设置，它就取代档位偏移算出的时刻。
            // 这是用户刚刚表达过的明确意图，优先级高于默认档位。
            if let Some(until) = snooze_until {
                scheduled_at = until;
            }

            out.push(FiringReminder {
                task_id: task.id.clone(),
                task_title: task.title.clone(),
                scheduled_at,
                offset_seconds: tier.offset_seconds,
                is_missed: is_missed(scheduled_at, now),
                last_fired_at,
                snooze_until,
            });
        }
    }

    Ok(out)
}

/// 启动时补发错过的提醒。
///
/// 这是"防漏机制"的实现：应用当时不在运行（关机、崩溃、被杀），
/// 那些提醒不能静默消失。这里把它们找出来、落库为"错过记录"并弹出补发通知。
///
/// 去重靠 `reminder.last_fired_at`：已经弹过的不会再弹。因此本函数是幂等的 ——
/// 多次启动不会产生重复的错过记录。
fn catch_up_missed(db: &Db) {
    let now = Local::now().fixed_offset();
    let repo = ReminderRepo::new(db);

    let candidates = match upcoming_reminders(db, now) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("读取错过提醒失败：{e}");
            return;
        }
    };

    let missed: Vec<FiringReminder> = candidates
        .into_iter()
        .filter(|r| r.is_missed)
        // **排除已经处理过的**。这一步不可省：一个提醒正常触发后会写下
        // `last_fired_at`，但它作为"过去的时刻"仍然会被重新推导出来。
        // 若不排除，每次启动都会把它当成"错过"再补发一遍 ——
        // 用户会看到"你错过了 3 个提醒"，其中一个其实准时弹过。
        .filter(|r| !r.already_handled())
        // 只补发最近 7 天内错过的。更早的提醒补发出来只会造成困扰 ——
        // 一周前该做的事，现在弹窗提醒已经没有意义，它们仍留在任务列表里。
        .filter(|r| (now - r.scheduled_at).num_days() <= 7)
        .collect();

    if missed.is_empty() {
        return;
    }

    // 先落库，再弹通知。顺序很重要：如果先弹通知后写库，中间崩溃会导致
    // 下次启动重复补发同一条。
    for r in &missed {
        if let Err(e) = repo.record_missed(&r.task_id, &r.task_title, &r.scheduled_at) {
            eprintln!("记录错过提醒失败：{e}");
            continue;
        }
        if let Err(e) = repo.mark_fired(r.task_id.as_str(), r.offset_seconds) {
            eprintln!("标记提醒已触发失败：{e}");
        }
    }

    println!("Todox 补发了 {} 个错过的提醒", missed.len());

    // 通知在触发时统一弹出；这里只负责把它们记录下来，
    // 由前端读取未确认的错过记录并显示"你错过了 X 个提醒"。
}

/// 真正触发一个提醒：标记已触发 + 弹出通知。
fn fire(db: &Db, firing: &FiringReminder) -> Result<(), String> {
    let settings = SettingsRepo::new(db).load().map_err(|e| e.to_string())?;
    let repo = ReminderRepo::new(db);

    // 先标记已触发，再弹通知。若顺序颠倒而中间失败，会重复弹同一条。
    repo.mark_fired(&firing.task_id, firing.offset_seconds)
        .map_err(|e| e.to_string())?;

    // **必须清除 snooze**。snooze_until 会取代档位偏移作为触发时刻，
    // 若弹完不清理，这个时刻永远停留在过去，调度器每次重算都会把它当成
    // "已过期待补发"，变成无限重复提醒。
    if firing.snooze_until.is_some() {
        if let Err(e) = repo.clear_snooze(&firing.task_id, firing.offset_seconds) {
            eprintln!("清除稍后提醒状态失败：{e}");
        }
    }

    if !settings.notifications_enabled {
        return Ok(());
    }
    if settings.in_quiet_hours(Local::now().hour()) {
        // 勿扰时段不弹，但**仍然标记为已触发**（上面已做），
        // 否则它会在勿扰结束后补弹一堆，那比不弹更烦人。
        return Ok(());
    }

    crate::notifier::show(&firing.task_title, firing.scheduled_at);
    Ok(())
}

// `Timelike` 提供 hour()
use chrono::Timelike;

/// 把一个 `NaiveDateTime` 附上本地时区。
///
/// 独立成函数是因为它涉及夏令时的两种边界（不存在/重复），
/// 散落在各处实现迟早会出现不一致的处理。
pub fn to_local(naive: NaiveDateTime) -> Option<DateTime<FixedOffset>> {
    match Local.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => Some(dt.fixed_offset()),
        chrono::LocalResult::Ambiguous(earlier, _) => Some(earlier.fixed_offset()),
        // 夏令时跳过的时刻：顺延一小时以保证总能得到有效时间
        chrono::LocalResult::None => (naive + chrono::Duration::hours(1))
            .and_local_timezone(Local)
            .single()
            .map(|dt| dt.fixed_offset()),
    }
}

#[cfg(test)]
mod tests;
