<script lang="ts">
  /**
   * 单条任务行。
   *
   * 关于勾选的语义：重复任务勾选后**不会**变成已完成状态，而是记录本次完成
   * 并推进到下一个周期。因此勾选框的视觉反馈对两类任务是不同的 ——
   * 重复任务勾选后仍显示为未勾选，但会闪一下表示"记下了"。
   * 若让它保持勾选状态，用户会以为这条任务已经彻底结束了。
   */

  import type { Task } from '$lib/stores/tasks.svelte';
  import {
    formatCountdown,
    formatDateLabel,
    formatTimeLabel,
    urgencyOf
  } from '$lib/utils/datetime';

  interface Props {
    task: Task;
    onDelete?: (id: string) => void;
    onToggle?: (task: Task) => void;
    /** 该任务是否正处于"刚刚完成"的短暂反馈状态 */
    justCompleted?: boolean;
  }

  let { task, onDelete, onToggle, justCompleted = false }: Props = $props();

  /** 重复任务的勾选是"完成一轮"，视觉上不应停留为选中态。 */
  const isRecurring = $derived(task.time_kind === 'recurring');

  /** 勾选框是否显示为选中。重复任务永远不显示为选中。 */
  const checked = $derived(!isRecurring && task.is_completed);

  /**
   * 时间标签文案。
   *
   * 全天任务只显示日期（"今天"），其余显示到分钟（"今天 14:30"）。
   * 这个区分直接对应四种时间类型的语义差异。
   */
  const timeLabel = $derived(
    task.time_kind === 'all_day'
      ? formatDateLabel(task.due_at)
      : formatTimeLabel(task.time_kind === 'before_deadline' ? task.deadline_at : task.due_at)
  );

  /** 截止型任务才显示倒计时，其余类型显示具体时刻即可。 */
  const countdown = $derived(
    task.time_kind === 'before_deadline' ? formatCountdown(task.deadline_at) : ''
  );

  const urgency = $derived(
    task.time_kind === 'before_deadline' ? urgencyOf(task.deadline_at) : 'relaxed'
  );

  /** 优先级色条：只有中/高优先级才显示，避免低优先级的任务也带装饰色。 */
  const priorityColor = $derived(
    task.priority === 3
      ? 'var(--color-danger)'
      : task.priority === 2
        ? 'var(--color-warning)'
        : null
  );

  /** 重复任务的标识文案。 */
  const recurringBadge = $derived(isRecurring ? '重复' : null);
</script>

<li class="row" class:has-countdown={!!countdown} class:completed={task.is_completed}>
  <button
    class="check"
    class:checked
    class:recurring={isRecurring}
    class:pulse={justCompleted}
    onclick={() => onToggle?.(task)}
    aria-pressed={checked}
    title={isRecurring
      ? '完成这一轮，并推进到下一个周期'
      : checked
        ? '取消完成'
        : '标记完成'}
    aria-label={isRecurring ? '完成这一轮' : '切换完成状态'}
  >
    <svg viewBox="0 0 24 24" width="12" height="12" fill="none" aria-hidden="true">
      <path
        d="m5 12.5 4.5 4.5L19 7.5"
        stroke="currentColor"
        stroke-width="3"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </svg>
  </button>

  <div class="body">
    <div class="title-line">
      {#if priorityColor}
        <span class="priority" style:background={priorityColor} aria-hidden="true"></span>
      {/if}
      <span class="title">{task.title}</span>
      {#if recurringBadge}
        <span class="badge">{recurringBadge}</span>
      {/if}
    </div>

    {#if task.note}
      <p class="note">{task.note}</p>
    {/if}
  </div>

  <div class="meta">
    {#if countdown}
      <span class="countdown tnum" data-urgency={urgency}>{countdown}</span>
    {/if}
    {#if timeLabel}
      <span class="time tnum">{timeLabel}</span>
    {/if}
  </div>

  {#if onDelete}
    <button class="delete" onclick={() => onDelete?.(task.id)} aria-label="删除任务">
      <svg viewBox="0 0 24 24" width="15" height="15" fill="none" aria-hidden="true">
        <path
          d="M6 6l12 12M18 6L6 18"
          stroke="currentColor"
          stroke-width="1.8"
          stroke-linecap="round"
        />
      </svg>
    </button>
  {/if}
</li>

<style>
  .row {
    display: flex;
    align-items: flex-start;
    gap: var(--space-3);
    /* 第四节硬性要求：行高不低于 44px */
    min-height: var(--row-height-min);
    padding: var(--space-3) var(--space-3) var(--space-3) var(--space-2);
    border-radius: var(--radius-card);
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .row:hover {
    background: var(--color-bg-hover);
  }

  /* ============ 勾选框 ============ */
  .check {
    flex-shrink: 0;
    width: 20px;
    height: 20px;
    margin-top: 1px;
    display: grid;
    place-items: center;
    border: 1.5px solid var(--color-border-strong);
    border-radius: var(--radius-full);
    color: transparent;
    cursor: pointer;
    transition:
      border-color var(--dur-fast) var(--ease-spring),
      background var(--dur-fast) var(--ease-spring),
      transform var(--dur-fast) var(--ease-spring);
  }

  .check:hover {
    border-color: var(--color-accent);
  }

  .check:active {
    transform: scale(0.9);
  }

  /* 已完成的普通任务：实心勾 + 标题加删除线 */
  .check.checked {
    background: var(--color-success);
    border-color: var(--color-success);
    color: #fff;
  }

  /* 重复任务的勾选框用主色描边而非填充色，暗示"这是一轮，不是终点" */
  .check.recurring:hover {
    border-color: var(--color-accent);
    background: color-mix(in srgb, var(--color-accent) 15%, transparent);
  }

  /* 刚完成时的短暂脉冲反馈。用缩放而非颜色变化，
     因为颜色变化在"勾选后立即回弹"的场景下几乎看不见。 */
  .check.pulse {
    animation: check-pulse var(--dur-slow) var(--ease-spring);
  }

  @keyframes check-pulse {
    0% { transform: scale(1); }
    40% { transform: scale(1.3); }
    100% { transform: scale(1); }
  }

  /* ============ 主体 ============ */
  .body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .title-line {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    min-width: 0;
  }

  .priority {
    flex-shrink: 0;
    width: 6px;
    height: 6px;
    border-radius: var(--radius-full);
  }

  .title {
    font-size: var(--text-item);
    line-height: var(--leading-tight);
    color: var(--color-text);
    /* 长标题换行而非截断：待办内容被截断会导致用户误判任务 */
    overflow-wrap: anywhere;
  }

  /* 已完成任务用删除线 + 弱化颜色，但仍留在列表里 ——
     第四节要求"未完成项不得直接消失"，已完成项同理，
     突然消失会让用户以为记录丢了。 */
  .completed .title {
    text-decoration: line-through;
    color: var(--color-text-tertiary);
  }

  .badge {
    flex-shrink: 0;
    padding: 1px 6px;
    border-radius: var(--radius-full);
    background: var(--color-bg-hover);
    color: var(--color-text-secondary);
    font-size: var(--text-mini);
    line-height: 1.6;
  }

  .note {
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  /* ============ 时间信息 ============ */
  .meta {
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    align-items: flex-end;
    gap: 1px;
    text-align: right;
  }

  .time {
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
    white-space: nowrap;
  }

  .countdown {
    font-size: var(--text-caption);
    font-weight: var(--weight-medium);
    white-space: nowrap;
  }

  /* 颜色只表达状态，对应 tokens.css 的紧迫度分档 */
  .countdown[data-urgency='relaxed'] {
    color: var(--color-urgency-relaxed);
    font-weight: var(--weight-regular);
  }
  .countdown[data-urgency='soon'] {
    color: var(--color-urgency-soon);
  }
  .countdown[data-urgency='overdue'] {
    color: var(--color-urgency-overdue);
  }

  /* 倒计时与具体时刻同时存在时，给具体时刻让出视觉权重 */
  .has-countdown .time {
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  /* ============ 删除按钮 ============ */
  /* 默认隐藏，悬浮或键盘聚焦时才出现：减少视觉噪音，同时不牺牲可访问性 */
  .delete {
    flex-shrink: 0;
    width: 24px;
    height: 24px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-sm);
    color: var(--color-text-tertiary);
    opacity: 0;
    transition:
      opacity var(--dur-fast) var(--ease-spring),
      color var(--dur-fast) var(--ease-spring),
      background var(--dur-fast) var(--ease-spring);
  }

  .row:hover .delete,
  .delete:focus-visible {
    opacity: 1;
  }

  .delete:hover {
    color: var(--color-danger);
    background: color-mix(in srgb, var(--color-danger) 12%, transparent);
  }
</style>
