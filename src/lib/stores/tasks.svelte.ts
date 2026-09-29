/**
 * 任务状态管理。
 *
 * 关于类型定义：本文件里的接口与 Rust 侧 `domain/task.rs` 是**手工对齐**的，
 * 没有自动生成。这么做是为了让前端的类型错误在 `svelte-check` 阶段就暴露，
 * 而不是等到运行时跨进程调用失败。
 *
 * 改动 Rust 侧的数据结构时，**必须同步改这里**。若将来字段增多到容易失配，
 * 再引入 ts-rs 之类的工具从 Rust 生成 TS 类型。
 */

import { invoke } from '@tauri-apps/api/core';

/** 对应 Rust 的 `TimeKind`，serde 使用 snake_case 序列化。 */
export type TimeKind = 'at_time' | 'before_deadline' | 'all_day' | 'recurring';

/** 对应 Rust 的 `Priority`，以整数传输。 */
export const Priority = {
  None: 0,
  Low: 1,
  Medium: 2,
  High: 3
} as const;
export type Priority = (typeof Priority)[keyof typeof Priority];

/** 对应 Rust 的 `Task`。 */
export interface Task {
  id: string;
  title: string;
  note: string | null;
  time_kind: TimeKind;
  due_at: string | null;
  deadline_at: string | null;
  recurrence_id: string | null;
  priority: Priority;
  is_completed: boolean;
  completed_at: string | null;
  sort_order: number;
  created_at: string;
  updated_at: string;
  deleted_at: string | null;
  revision: number;
}

/** 对应 Rust 的 `NewTask`。 */
export interface NewTask {
  title: string;
  note: string | null;
  time_kind: TimeKind;
  due_at: string | null;
  deadline_at: string | null;
  recurrence_id: string | null;
  priority: Priority;
}

/** 对应 Rust 的 `ParsePreview`。 */
export interface ParsePreview {
  title: string;
  time_label: string | null;
  recurrence_label: string | null;
  time_kind: TimeKind;
  has_time: boolean;
  matched_text: string | null;
}

/** 对应 Rust 的 `TaskCompletion`。 */
export interface TaskCompletion {
  id: string;
  task_id: string;
  occurrence_at: string | null;
  completed_at: string;
  created_at: string;
  updated_at: string;
  deleted_at: string | null;
  revision: number;
}

/** 对应 Rust 的 `MissedReminderRow`。 */
export interface MissedReminder {
  id: string;
  task_id: string;
  task_title: string;
  scheduled_at: string;
  detected_at: string;
  is_acknowledged: boolean;
}

/**
 * 对应 Rust 的 `TaskEdit`。
 *
 * 所有字段可选，`undefined` 表示"不改这一项"。
 *
 * 关于 `due_at` 的 `null` 与 `undefined` 的差别：
 *   `undefined` → 不改动
 *   `null`      → 清空该时间（变成收件箱任务）
 * 这个区分对应 Rust 侧的 `Option<Option<String>>`，是刻意的设计 ——
 * "把任务的时间删掉"必须能与"不动它"分开表达。
 */
export interface TaskEdit {
  title?: string;
  note?: string;
  priority?: Priority;
  time_kind?: TimeKind;
  due_at?: string | null;
  deadline_at?: string | null;
}

/** 构造一条"收件箱"任务：只带标题，没有安排时间。 */
export function inboxTask(title: string): NewTask {
  return {
    title,
    note: null,
    time_kind: 'all_day',
    due_at: null,
    deadline_at: null,
    recurrence_id: null,
    priority: Priority.None
  };
}

/**
 * 把跨进程调用抛出的错误转成可展示的文本。
 *
 * Rust 侧的命令统一返回 `Result<T, String>`，因此正常情况下拿到的已经是
 * 中文消息。这里额外兜底，是为了不让界面上出现 `[object Object]` 这类
 * 无法理解的内容 —— 那会让用户以为程序坏了。
 */
export function toMessage(err: unknown): string {
  if (typeof err === 'string') return err;
  if (err instanceof Error) return err.message;
  return '发生了未知错误，请重试';
}

class TaskStore {
  tasks = $state<Task[]>([]);
  loading = $state(false);
  error = $state<string | null>(null);

  /**
   * 未确认的错过提醒。
   *
   * 界面据此显示"你错过了 X 个提醒"。它由调度器在启动时落库，
   * 因此跨重启仍然存在 —— 这正是"防漏"能成立的前提。
   */
  missed = $state<MissedReminder[]>([]);

  /** 最近一次完成的任务 ID，用于播放短暂的勾选反馈动画。
   *
   * 之所以需要它：重复任务勾选后状态会立刻回到"未完成"（因为它的发生时间
   * 被推进了），若没有这个短反馈，用户会以为自己没点中。
   */
  justCompleted = $state<string | null>(null);

  /**
   * 未完成任务数。侧边栏徽标使用。
   *
   * 从已加载的列表派生而非单独查数据库：列表本身就是要展示的数据，
   * 再查一次库属于重复劳动（本应用的内存与 CPU 指标是硬要求）。
   */
  unfinished = $derived(this.tasks.filter((t) => !t.is_completed).length);

  /** 从数据库重新加载任务列表与错过提醒。 */
  async load(includeCompleted = true): Promise<void> {
    this.loading = true;
    this.error = null;
    try {
      const [tasks, missed] = await Promise.all([
        invoke<Task[]>('list_tasks', { includeCompleted }),
        invoke<MissedReminder[]>('pending_missed').catch(() => [] as MissedReminder[])
      ]);
      this.tasks = tasks;
      this.missed = missed;
    } catch (err) {
      this.error = toMessage(err);
    } finally {
      this.loading = false;
    }
  }

  /** 确认一条错过提醒（用户点"知道了"）。 */
  async acknowledgeMissed(id: string): Promise<void> {
    try {
      await invoke('acknowledge_missed', { id });
      this.missed = this.missed.filter((m) => m.id !== id);
    } catch (err) {
      this.error = toMessage(err);
    }
  }

  /** 全部确认。 */
  async acknowledgeAllMissed(): Promise<void> {
    try {
      await invoke<number>('acknowledge_all_missed');
      this.missed = [];
    } catch (err) {
      this.error = toMessage(err);
    }
  }

  /**
   * 修改任务。
   *
   * 时间类型变化时后端会重建提醒档位（时间点任务只有到点提醒，
   * 截止型任务有三级分级提醒），因此调用方不需要额外处理。
   */
  async update(id: string, edit: TaskEdit): Promise<boolean> {
    this.error = null;
    try {
      await invoke<Task>('update_task', { id, edit });
      await this.load();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /** 把某个提醒推迟若干分钟。 */
  async snooze(taskId: string, offsetMinutes: number, minutes: number): Promise<boolean> {
    this.error = null;
    try {
      await invoke('snooze_reminder', {
        taskId,
        offsetMinutes,
        minutes
      });
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /** 新建任务。成功后重新加载列表，保证界面与数据库一致。 */
  async create(input: NewTask): Promise<boolean> {
    this.error = null;
    try {
      await invoke<Task>('create_task', { input });
      await this.load();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /**
   * 从自然语言文本创建任务。
   *
   * 解析与建库都在 Rust 侧一次完成，避免"先解析再创建"两次调用之间
   * 出现不一致（例如解析成功但创建重复规则失败，留下孤儿规则）。
   */
  async createFromText(text: string): Promise<boolean> {
    this.error = null;
    try {
      await invoke<Task>('create_task_from_text', { text });
      await this.load();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /** 解析预览。不落库，仅用于输入框的实时提示。 */
  async preview(text: string): Promise<ParsePreview | null> {
    try {
      return await invoke<ParsePreview>('parse_input', { text });
    } catch {
      // 预览失败不应打断输入。静默返回 null，让界面不显示时间标签即可 ——
      // 在这里弹错误会非常打扰，因为用户每敲一个字都会触发一次预览。
      return null;
    }
  }

  /**
   * 切换完成状态。
   *
   * 对重复任务，这是"完成这一轮"：后端会记录完成并推进发生时间，
   * 任务本身仍保持未完成。前端不应假定勾选就等于完成。
   */
  async toggleComplete(task: Task): Promise<boolean> {
    this.error = null;
    try {
      if (task.time_kind === 'recurring') {
        await invoke('complete_task', { id: task.id });
      } else if (task.is_completed) {
        await invoke('uncomplete_task', { id: task.id });
      } else {
        await invoke('complete_task', { id: task.id });
      }

      // 完成后播放一次短暂反馈
      this.justCompleted = task.id;
      setTimeout(() => {
        if (this.justCompleted === task.id) this.justCompleted = null;
      }, 400);

      await this.load();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /** 软删除。界面层负责提供撤销入口。 */
  async remove(id: string): Promise<boolean> {
    this.error = null;
    try {
      await invoke('delete_task', { id });
      await this.load();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /** 撤销删除。依赖后端的软删除设计，物理删除无法做到这一点。 */
  async restore(id: string): Promise<boolean> {
    this.error = null;
    try {
      await invoke<Task>('restore_task', { id });
      await this.load();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  clearError(): void {
    this.error = null;
  }
}

export const taskStore = new TaskStore();
