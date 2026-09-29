-- ============================================================================
-- Todox 数据库 schema
--
-- 迁移策略：文件末尾的 user_version 必须等于 SCHEMA_VERSION 常量。
-- 应用启动时比对两者，不一致则判定需要迁移。阶段二只有 v1，
-- 首次创建时把 user_version 置为 1。
--
-- 设计前提（见 README「未来接入云端同步的方案」）：
--   1. 主键用 TEXT 存 UUID，不用 INTEGER 自增 —— 自增 ID 在多设备上必然碰撞。
--   2. 一律软删除（deleted_at），不物理删除 —— 硬删除无法把"删除"这个动作
--      同步给其他设备，会表现为"删掉的记录又回来了"。墓碑记录是同步的必要条件。
--   3. 每张业务表都有 updated_at 与 revision，作为冲突检测的输入。
--   4. 时间统一存 RFC3339 文本（带时区偏移）。选它而非 epoch 整数，
--      是因为可以直接读懂、按字典序排序，调试成本远低于一堆大整数。
-- ============================================================================

-- ---------------------------------------------------------------------------
-- 重复规则
--
-- 用"类型 + 参数"的通用表结构，而不是给每种频率建一张表。
-- until_date 与 max_count 是二选一的结束条件，两者可同时为 NULL（永不结束）。
-- 重复任务的下一次发生时间永远由本表实时推导，不物化存储 ——
-- 物化状态在进程被杀或系统休眠后必然与现实脱节。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS recurrence_rule (
    id              TEXT PRIMARY KEY NOT NULL,
    freq            TEXT NOT NULL CHECK (freq IN (
                        'daily', 'weekdays', 'weekly', 'monthly',
                        'every_n_days', 'every_n_weeks'
                    )),
    interval        INTEGER NOT NULL DEFAULT 1 CHECK (interval >= 1),
    -- 星期掩码，按位表示周一至周日（bit0=周一 ... bit6=周日）。
    -- 只在 freq='weekly' 时有意义，例如 0b0000001 = 1 = 仅周一。
    by_weekdays     INTEGER,
    -- 每月第几日，1-31。只在 freq='monthly' 时有意义。
    -- 值为 31 而该月只有 30 天时如何处理，由 domain/recurrence.rs 定义（取该月最后一天）。
    by_monthday     INTEGER CHECK (by_monthday IS NULL OR (by_monthday BETWEEN 1 AND 31)),
    until_date      TEXT,
    max_count       INTEGER CHECK (max_count IS NULL OR max_count >= 1),
    -- 每次发生的时间点（HH:MM），重复任务靠它确定当天的触发时刻。
    at_time_of_day  TEXT,
    tz              TEXT,

    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

-- ---------------------------------------------------------------------------
-- 任务
--
-- 四种时间类型共存于一张表，用 time_kind 区分，各自用独立字段承载，
-- 而不是塞进一个含义模糊的 scheduled_at：
--   at_time          → due_at
--   before_deadline  → deadline_at
--   all_day          → due_at（只取日期部分）
--   recurring        → recurrence_id 指向规则表，due_at 存首次发生时间
-- 这样"倒计时按哪种时间算"始终是明确的，不需要在读取时反推。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS task (
    id              TEXT PRIMARY KEY NOT NULL,
    title           TEXT NOT NULL,
    note            TEXT,
    time_kind       TEXT NOT NULL CHECK (time_kind IN (
                        'at_time', 'before_deadline', 'all_day', 'recurring'
                    )),
    -- 执行/发生时刻，RFC3339。at_time / all_day / recurring 使用。
    due_at          TEXT,
    -- 截止时刻，RFC3339。仅 before_deadline 使用。
    deadline_at     TEXT,
    recurrence_id   TEXT REFERENCES recurrence_rule(id) ON DELETE SET NULL,

    -- 优先级：0 无 / 1 低 / 2 中 / 3 高
    priority        INTEGER NOT NULL DEFAULT 0 CHECK (priority BETWEEN 0 AND 3),

    -- 完成状态。重复任务的"完成"语义不同：它记录在 task_completion 里，
    -- 本字段对重复任务恒为 0，避免"勾一次整个重复序列就结束了"的错误。
    is_completed    INTEGER NOT NULL DEFAULT 0 CHECK (is_completed IN (0, 1)),
    completed_at    TEXT,

    -- 排序用的手动拖拽位置，留待阶段五使用
    sort_order      INTEGER NOT NULL DEFAULT 0,

    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

-- 常用查询路径：今天视图按 due_at 扫描未完成任务
CREATE INDEX IF NOT EXISTS idx_task_due        ON task(due_at)      WHERE deleted_at IS NULL AND is_completed = 0;
CREATE INDEX IF NOT EXISTS idx_task_deadline   ON task(deadline_at) WHERE deleted_at IS NULL AND is_completed = 0;
CREATE INDEX IF NOT EXISTS idx_task_kind       ON task(time_kind)   WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_task_recurrence ON task(recurrence_id);

-- ---------------------------------------------------------------------------
-- 完成记录
--
-- 独立成表而不是在 task 上打一个布尔标记，有两个不可替代的理由：
--   1. 重复任务需要"每次完成"的流水才能推进周期并支撑统计。
--   2. 同步时本表是"只追加"的，天然无冲突，直接取并集即可；
--      而布尔标记的并发更新需要真正的冲突解决。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS task_completion (
    id              TEXT PRIMARY KEY NOT NULL,
    task_id         TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    -- 被完成的那一次计划发生的时刻（区别于 completed_at 实际完成时刻）。
    -- 重复任务靠它判断"是哪一轮被完成了"。
    occurrence_at   TEXT,
    completed_at    TEXT NOT NULL,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_completion_task ON task_completion(task_id, completed_at);
CREATE INDEX IF NOT EXISTS idx_completion_time ON task_completion(completed_at) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- 提醒档位
--
-- offset_seconds 表示相对任务时间基准的偏移，单位是**秒**，负值代表"提前"。
-- 默认分级策略「截止前 1 天 / 前 1 小时 / 到点」对应 -86400 / -3600 / 0。
--
-- 为什么用秒而不是分钟：用户会用「10 秒后提醒我」来快速验证提醒是否工作，
-- 若单位是分钟，10 秒会被截断成 0，表现为"输入了时间却没有任何提醒"。
--
-- 只存偏移而不存绝对触发时刻，是为了让"改任务时间"自动带动提醒时间平移，
-- 无需同步更新多条提醒记录。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS reminder (
    id              TEXT PRIMARY KEY NOT NULL,
    task_id         TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    offset_seconds  INTEGER NOT NULL,
    -- 用户一次性"稍后提醒"产生的绝对时刻。存在时优先于 offset_seconds。
    snooze_until    TEXT,
    is_enabled      INTEGER NOT NULL DEFAULT 1 CHECK (is_enabled IN (0, 1)),
    -- 最近一次实际弹出的时刻，用于避免重复提醒与计算错过补发
    last_fired_at   TEXT,

    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_reminder_task ON reminder(task_id) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- 标签
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS tag (
    id              TEXT PRIMARY KEY NOT NULL,
    name            TEXT NOT NULL,
    -- 颜色只用于区分，不承载状态语义，因此允许自定义
    color           TEXT,
    sort_order      INTEGER NOT NULL DEFAULT 0,

    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

-- 唯一性只在未删除的行上生效：删掉的标签名应可被重新使用
CREATE UNIQUE INDEX IF NOT EXISTS idx_tag_name ON tag(name) WHERE deleted_at IS NULL;

CREATE TABLE IF NOT EXISTS task_tag (
    task_id         TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    tag_id          TEXT NOT NULL REFERENCES tag(id)  ON DELETE CASCADE,
    created_at      TEXT NOT NULL,
    PRIMARY KEY (task_id, tag_id)
);

-- ---------------------------------------------------------------------------
-- 子任务
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS subtask (
    id              TEXT PRIMARY KEY NOT NULL,
    task_id         TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    title           TEXT NOT NULL,
    is_completed    INTEGER NOT NULL DEFAULT 0 CHECK (is_completed IN (0, 1)),
    sort_order      INTEGER NOT NULL DEFAULT 0,

    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_subtask_task ON subtask(task_id, sort_order) WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------------------
-- 设置（键值对）
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS setting (
    key             TEXT PRIMARY KEY NOT NULL,
    value           TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);

-- ---------------------------------------------------------------------------
-- 被错过的提醒
--
-- 应用未运行时（用户关机、进程被杀、系统休眠）到期的提醒会落到这张表，
-- 而不是被静默丢弃。启动时读取未处理的行，向用户明确提示"你错过了 X 个提醒"。
--
-- 为什么必须持久化而不能只放在内存里：错过这件事本身就是"进程不在"导致的，
-- 内存里的标记随进程一起消失了，唯一能跨重启保留的地方就是磁盘。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS missed_reminder (
    id              TEXT PRIMARY KEY NOT NULL,
    task_id         TEXT NOT NULL REFERENCES task(id) ON DELETE CASCADE,
    -- 任务标题快照。任务可能随后被改名或删除，但"当时错过的是什么"不应改变。
    task_title      TEXT NOT NULL,
    -- 本应触发的时刻
    scheduled_at    TEXT NOT NULL,
    -- 实际被发现错过的时刻（通常是应用下一次启动时）
    detected_at     TEXT NOT NULL,
    -- 用户是否已处理（点击"知道了"或完成后置位）
    is_acknowledged INTEGER NOT NULL DEFAULT 0 CHECK (is_acknowledged IN (0, 1)),

    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL,
    deleted_at      TEXT,
    revision        INTEGER NOT NULL DEFAULT 1
);

CREATE INDEX IF NOT EXISTS idx_missed_pending
    ON missed_reminder(scheduled_at)
    WHERE deleted_at IS NULL AND is_acknowledged = 0;

-- ---------------------------------------------------------------------------
-- 变更流水（为同步预留）
--
-- 本地每次写操作追加一条记录。未来接云端时，pull 只需按 since 游标增量拉取，
-- 无需全表对比。现在只写入不消费，成本极低。
-- ---------------------------------------------------------------------------
CREATE TABLE IF NOT EXISTS changelog (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    entity          TEXT NOT NULL,
    entity_id       TEXT NOT NULL,
    op              TEXT NOT NULL CHECK (op IN ('insert', 'update', 'delete')),
    changed_at      TEXT NOT NULL,
    -- 同步成功后回填，未同步的记录此列为 NULL
    synced_at       TEXT
);

CREATE INDEX IF NOT EXISTS idx_changelog_pending ON changelog(changed_at) WHERE synced_at IS NULL;
