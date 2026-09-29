<script lang="ts">
  /**
   * 日历视图：月视图 + 选中某天的日视图。
   *
   * 关于日期计算的取舍：月份天数与首日星期都不用"记住 30/31 天的表"，
   * 而是用 `nextMonth(1).getDate()` 与 Date 构造函数自动归一化来推导。
   * 手写月份天数表在闰年上出错是经典事故，而这里完全不需要那张表。
   */

  import TaskRow from '$lib/components/TaskRow.svelte';
  import { taskStore, type Task } from '$lib/stores/tasks.svelte';
  import { dayDiff, parseTime } from '$lib/utils/datetime';

  interface Props {
    onToggle?: (task: Task) => void;
    onDelete?: (id: string) => void;
  }

  let { onToggle, onDelete }: Props = $props();

  /** 当前显示的月份（该月 1 日）。 */
  let viewMonth = $state(new Date(new Date().getFullYear(), new Date().getMonth(), 1));

  /** 选中的日期（用于下方的日视图）。默认今天。 */
  let selected = $state(new Date());

  const WEEKDAYS = ['一', '二', '三', '四', '五', '六', '日'];

  /** 把 Date 归一化成 `YYYY-MM-DD`，用于跨组件比较日期。 */
  function key(d: Date): string {
    const p = (n: number) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  }

  /** 任务的时间基准：截止型看 deadline_at，其余看 due_at。 */
  function anchorOf(task: Task): Date | null {
    return parseTime(task.time_kind === 'before_deadline' ? task.deadline_at : task.due_at);
  }

  /**
   * 按日期分组的任务索引。
   *
   * 一次性建好索引而不是"每天遍历一遍任务列表"：后者在一个 6×7 的网格里
   * 会做 42 次全量遍历，任务多时明显卡顿。
   */
  const byDate = $derived.by(() => {
    const map = new Map<string, Task[]>();
    for (const t of taskStore.tasks) {
      const a = anchorOf(t);
      if (!a) continue;
      const k = key(a);
      const list = map.get(k);
      if (list) list.push(t);
      else map.set(k, [t]);
    }
    return map;
  });

  /**
   * 生成网格所需的全部日子。
   *
   * 从当月 1 日往前退到所在周的周一，末尾补到整周。
   * `getDay()` 里周日是 0，而我们把周一当第一天，因此要转换：
   * `(getDay() + 6) % 7` 得到"距周一的天数"。
   */
  const gridDays = $derived.by(() => {
    const y = viewMonth.getFullYear();
    const m = viewMonth.getMonth();

    const first = new Date(y, m, 1);
    const leading = (first.getDay() + 6) % 7;
    const start = new Date(y, m, 1 - leading);

    // 当月天数：下月 1 日往前退一天。
    // 不用手写 30/31 天表 —— 那在闰年二月会出错。
    const daysInMonth = new Date(y, m + 1, 0).getDate();
    const total = Math.ceil((leading + daysInMonth) / 7) * 7;

    const out: { date: Date; inMonth: boolean; isToday: boolean }[] = [];
    const todayKey = key(new Date());

    for (let i = 0; i < total; i++) {
      const d = new Date(start.getFullYear(), start.getMonth(), start.getDate() + i);
      out.push({
        date: d,
        inMonth: d.getMonth() === m,
        isToday: key(d) === todayKey
      });
    }
    return out;
  });

  const monthLabel = $derived(
    new Intl.DateTimeFormat('zh-CN', { year: 'numeric', month: 'long' }).format(viewMonth)
  );

  const selectedLabel = $derived(
    new Intl.DateTimeFormat('zh-CN', {
      month: 'long',
      day: 'numeric',
      weekday: 'long'
    }).format(selected)
  );

  /** 选中日的任务。 */
  const selectedTasks = $derived(byDate.get(key(selected)) ?? []);

  function shiftMonth(delta: number) {
    viewMonth = new Date(viewMonth.getFullYear(), viewMonth.getMonth() + delta, 1);
  }

  function goToday() {
    const now = new Date();
    viewMonth = new Date(now.getFullYear(), now.getMonth(), 1);
    selected = now;
  }

  function pick(d: Date) {
    selected = d;
    // 点到相邻月份的日子时顺带切月，避免"点了没反应"的困惑
    if (d.getMonth() !== viewMonth.getMonth()) {
      viewMonth = new Date(d.getFullYear(), d.getMonth(), 1);
    }
  }

  /** 逾期判断：该任务的时间基准已过去且未完成。 */
  function isOverdue(t: Task): boolean {
    const a = anchorOf(t);
    return !!a && !t.is_completed && a.getTime() < Date.now();
  }
</script>

<div class="page">
  <header class="page-head">
    <div class="head-row">
      <h1 class="page-title">日历</h1>
      <div class="nav-group">
        <button class="icon-btn" onclick={() => shiftMonth(-1)} aria-label="上个月">‹</button>
        <span class="month-label tnum">{monthLabel}</span>
        <button class="icon-btn" onclick={() => shiftMonth(1)} aria-label="下个月">›</button>
        <button class="today-btn" onclick={goToday}>今天</button>
      </div>
    </div>
  </header>

  <!-- ============ 月视图 ============ -->
  <section class="calendar">
    <div class="weekday-row">
      {#each WEEKDAYS as w}
        <span class="weekday">{w}</span>
      {/each}
    </div>

    <div class="grid">
      {#each gridDays as cell (cell.date.getTime())}
        {@const tasks = byDate.get(key(cell.date)) ?? []}
        {@const done = tasks.filter((t) => t.is_completed).length}
        <button
          class="cell"
          class:out={!cell.inMonth}
          class:today={cell.isToday}
          class:selected={key(cell.date) === key(selected)}
          onclick={() => pick(cell.date)}
          aria-label="{cell.date.getMonth() + 1} 月 {cell.date.getDate()} 日，{tasks.length} 个任务"
        >
          <span class="day tnum">{cell.date.getDate()}</span>

          {#if tasks.length > 0}
            <span class="dots">
              <!-- 最多显示 3 个圆点，其余用数字表示：
                   一个格子里画 10 个点既看不清也没有信息量 -->
              {#each tasks.slice(0, 3) as t (t.id)}
                <span
                  class="dot"
                  class:done={t.is_completed}
                  class:overdue={isOverdue(t)}
                ></span>
              {/each}
              {#if tasks.length > 3}
                <span class="more tnum">+{tasks.length - 3}</span>
              {/if}
            </span>
          {/if}

          {#if done > 0 && done === tasks.length}
            <span class="all-done" aria-hidden="true">✓</span>
          {/if}
        </button>
      {/each}
    </div>
  </section>

  <!-- ============ 日视图 ============ -->
  <section class="day-panel">
    <h2 class="day-title">{selectedLabel}</h2>

    {#if selectedTasks.length === 0}
      <p class="empty-line">这一天没有安排。</p>
    {:else}
      <ul class="task-list">
        {#each selectedTasks as task (task.id)}
          <TaskRow
            {task}
            onDelete={onDelete}
            onToggle={onToggle}
            justCompleted={taskStore.justCompleted === task.id}
          />
        {/each}
      </ul>
    {/if}
  </section>

  <p class="legend">
    <span class="legend-item"><span class="dot"></span>未完成</span>
    <span class="legend-item"><span class="dot overdue"></span>已逾期</span>
    <span class="legend-item"><span class="dot done"></span>已完成</span>
  </p>
</div>

<style>
  .page {
    width: 100%;
    max-width: var(--content-max-width);
    padding: var(--space-12) var(--space-8) var(--space-8);
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
  }

  .page-head {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }

  .head-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
    flex-wrap: wrap;
  }

  .page-title {
    font-size: var(--text-title);
    line-height: var(--leading-tight);
    font-weight: var(--weight-semibold);
    letter-spacing: -0.02em;
  }

  .nav-group {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .month-label {
    font-size: var(--text-item);
    font-weight: var(--weight-medium);
    min-width: 7ch;
    text-align: center;
  }

  .icon-btn {
    width: 28px;
    height: 28px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-sm);
    color: var(--color-text-secondary);
    font-size: 18px;
    line-height: 1;
  }

  .icon-btn:hover {
    background: var(--color-bg-hover);
    color: var(--color-text);
  }

  .today-btn {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    font-size: var(--text-caption);
    color: var(--color-text);
  }

  .today-btn:hover {
    background: var(--color-bg-hover);
  }

  /* ================= 月视图 ================= */
  .calendar {
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-card);
    padding: var(--space-3);
    box-shadow: var(--shadow-sm);
  }

  .weekday-row {
    display: grid;
    grid-template-columns: repeat(7, 1fr);
    padding-bottom: var(--space-2);
  }

  .weekday {
    text-align: center;
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(7, 1fr);
    gap: 2px;
  }

  .cell {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    gap: 3px;
    /* 日历格子必须够大才能点得准，但也别撑得太高把日视图挤下去 */
    min-height: 52px;
    padding: var(--space-1);
    border-radius: var(--radius-sm);
    color: var(--color-text);
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .cell:hover {
    background: var(--color-bg-hover);
  }

  /* 非本月的日子弱化，但仍可点击（点了会切月） */
  .cell.out {
    color: var(--color-text-tertiary);
  }

  .cell.today .day {
    color: var(--color-accent);
    font-weight: var(--weight-semibold);
  }

  .cell.selected {
    background: var(--color-bg-selected);
  }

  .day {
    font-size: var(--text-caption);
    line-height: 1;
  }

  .dots {
    display: flex;
    align-items: center;
    gap: 2px;
    height: 6px;
  }

  .dot {
    width: 5px;
    height: 5px;
    border-radius: var(--radius-full);
    background: var(--color-text-tertiary);
    flex-shrink: 0;
  }

  .dot.overdue {
    background: var(--color-danger);
  }

  .dot.done {
    background: var(--color-success);
  }

  .more {
    font-size: 9px;
    color: var(--color-text-tertiary);
    line-height: 1;
  }

  .all-done {
    position: absolute;
    top: 2px;
    right: 4px;
    font-size: 9px;
    color: var(--color-success);
  }

  /* ================= 日视图 ================= */
  .day-panel {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }

  .day-title {
    font-size: var(--text-heading);
    font-weight: var(--weight-medium);
  }

  .task-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  .empty-line {
    padding: var(--space-6) 0;
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .legend {
    display: flex;
    gap: var(--space-4);
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  .legend-item {
    display: flex;
    align-items: center;
    gap: var(--space-1);
  }
</style>
