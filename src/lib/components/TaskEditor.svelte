<script lang="ts">
  /**
   * 任务编辑面板。
   *
   * 存在的理由：自然语言解析器不产生「截止型任务」（没有"在 X 之前做完"
   * 这类表达的解析），而分级提醒恰恰是为截止型任务设计的。
   * 没有这个表单，那套能力就无法从界面触达。
   *
   * 表单同时覆盖四种时间类型，因为它们是同一个数据模型的不同投影 ——
   * 用四个独立表单会让"把时间点任务改成截止型"变成不可能的操作。
   */

  import { taskStore, Priority, type Task, type TaskEdit, type TimeKind } from '$lib/stores/tasks.svelte';

  interface Props {
    task: Task;
    onClose: () => void;
    onSaved?: () => void;
  }

  let { task, onClose, onSaved }: Props = $props();

  // ---------- 表单状态 ----------
  //
  // 表单需要一份**可编辑的副本**，而不是直接改 prop：用户在输入过程中若底层
  // 任务数据被刷新（例如后台完成了一轮重复任务），直接绑定 prop 会让用户
  // 正在敲的字被覆盖。
  //
  // Svelte 会警告"这里只捕获了初始值"。那个警告针对的是"误以为能跟随更新"
  // 的写法，而这里恰恰是刻意只取初始值，因此显式忽略并写明理由 ——
  // 比为了绕过检查器而改写代码更诚实。
  // svelte-ignore state_referenced_locally
  let title = $state(task.title);
  // svelte-ignore state_referenced_locally
  let note = $state(task.note ?? '');
  // svelte-ignore state_referenced_locally
  let priority = $state<Priority>(task.priority);
  // svelte-ignore state_referenced_locally
  let timeKind = $state<TimeKind>(task.time_kind);

  /**
   * 日期与时间分开输入。
   *
   * 原生的 `datetime-local` 在 Windows 上渲染成一个包含日期与时间的复合控件，
   * 但在不同浏览器内核下外观差异很大，且无法分别设置。拆成两个输入
   * 视觉效果更可控，也更容易让用户理解"我只想改时间，不改日期"。
   */
  let dateStr = $state('');
  let timeStr = $state('');

  let saving = $state(false);

  /**
   * 把 RFC3339 时间戳拆成本地日期与时间字符串。
   *
   * 必须用本地时间分量（getFullYear 等）而不是 toISOString()：
   * 后者会转成 UTC，东八区的 00:30 会变成前一天的 16:30，日期直接错一天。
   */
  function splitLocal(iso: string | null): { date: string; time: string } {
    if (!iso) return { date: '', time: '' };
    const d = new Date(iso);
    if (Number.isNaN(d.getTime())) return { date: '', time: '' };

    const p = (n: number) => String(n).padStart(2, '0');
    return {
      date: `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`,
      time: `${p(d.getHours())}:${p(d.getMinutes())}`
    };
  }

  /** 时间类型改变时，用当前基准时间的值初始化输入框。 */
  function syncFromTask() {
    const anchor = timeKind === 'before_deadline' ? task.deadline_at : task.due_at;
    const parts = splitLocal(anchor);
    dateStr = parts.date;
    timeStr = parts.time;
  }

  $effect(() => {
    // 仅在切换类型时重新同步，避免用户正在输入时被覆盖
    void timeKind;
    syncFromTask();
  });

  /** 把日期与时间合成 RFC3339 字符串。缺日期则返回 null。 */
  function compose(needTime: boolean): string | null {
    if (!dateStr) return null;
    const [y, m, d] = dateStr.split('-').map(Number);
    const [hh, mm] = needTime && timeStr ? timeStr.split(':').map(Number) : [0, 0];
    const local = new Date(y, (m ?? 1) - 1, d ?? 1, hh ?? 0, mm ?? 0, 0);
    if (Number.isNaN(local.getTime())) return null;
    // toISOString 给出 UTC 的 Z 格式，这是合法的 RFC3339，
    // 后端会保留它并在展示时转成本地时间。
    return local.toISOString();
  }

  const canSave = $derived(title.trim().length > 0 && !saving);

  async function save() {
    if (!canSave) return;
    saving = true;

    const edit: TaskEdit = {
      title: title.trim(),
      note: note.trim(),
      priority,
      time_kind: timeKind
    };

    // 按类型决定写哪个时间字段。
    // 用 `null`（而非省略）表示"清空" —— 这样从「截止型」改回「全天」时，
    // 遗留的 deadline_at 会被清掉，而不是留在库里成为无用数据。
    switch (timeKind) {
      case 'before_deadline':
        edit.deadline_at = compose(true);
        edit.due_at = null;
        break;
      case 'at_time':
      case 'recurring':
        edit.due_at = compose(true);
        edit.deadline_at = null;
        break;
      case 'all_day':
        edit.due_at = compose(false);
        edit.deadline_at = null;
        break;
    }

    const ok = await taskStore.update(task.id, edit);
    saving = false;
    if (ok) {
      onSaved?.();
      onClose();
    }
  }

  const KIND_OPTIONS: { value: TimeKind; label: string; hint: string }[] = [
    { value: 'at_time', label: '时间点', hint: '某时刻要做，到点提醒' },
    { value: 'before_deadline', label: '截止前完成', hint: '在截止时间前做完，分级提醒' },
    { value: 'all_day', label: '全天', hint: '当天做即可，按设定钟点提醒' },
    { value: 'recurring', label: '重复', hint: '按重复规则反复出现' }
  ];

  const needsTime = $derived(timeKind !== 'all_day');
</script>

<!-- 遮罩点击关闭：比"必须找到取消按钮"更符合直觉 -->
<div
  class="overlay"
  role="button"
  tabindex="-1"
  aria-label="关闭编辑面板"
  onclick={onClose}
  onkeydown={(e) => e.key === 'Escape' && onClose()}
></div>

<div class="panel" role="dialog" aria-modal="true" aria-label="编辑任务">
  <header class="panel-head">
    <h2 class="panel-title">编辑任务</h2>
  </header>

  <div class="field">
    <label class="label" for="edit-title">标题</label>
    <input id="edit-title" class="input" bind:value={title} placeholder="要做的事" />
  </div>

  <div class="field">
    <span class="label">时间类型</span>
    <div class="kinds">
      {#each KIND_OPTIONS as opt (opt.value)}
        <button
          class="kind"
          class:on={timeKind === opt.value}
          onclick={() => (timeKind = opt.value)}
          title={opt.hint}
        >
          <span class="kind-label">{opt.label}</span>
          <span class="kind-hint">{opt.hint}</span>
        </button>
      {/each}
    </div>
    {#if timeKind === 'recurring'}
      <p class="note">
        重复规则（每周几、间隔）目前只能通过自然语言设置。若要改成重复任务，
        建议删除后重新输入，例如「每周一早上9点 提交周报」。
      </p>
    {/if}
  </div>

  <div class="field">
    <span class="label">{timeKind === 'before_deadline' ? '截止时间' : '时间'}</span>
    <div class="datetime">
      <input class="input" type="date" bind:value={dateStr} />
      {#if needsTime}
        <input class="input time-input" type="time" bind:value={timeStr} />
      {/if}
    </div>
    {#if !dateStr}
      <p class="note">留空则存入收件箱（没有时间）。</p>
    {/if}
  </div>

  <div class="field">
    <span class="label">优先级</span>
    <div class="segmented">
      {#each [[Priority.None, '无'], [Priority.Low, '低'], [Priority.Medium, '中'], [Priority.High, '高']] as [value, label] (value)}
        <button class="seg" class:on={priority === value} onclick={() => (priority = value as Priority)}>
          {label}
        </button>
      {/each}
    </div>
  </div>

  <div class="field">
    <label class="label" for="edit-note">备注</label>
    <textarea
      id="edit-note"
      class="input textarea"
      bind:value={note}
      rows="3"
      placeholder="补充说明（可选）"
    ></textarea>
  </div>

  {#if taskStore.error}
    <p class="error">{taskStore.error}</p>
  {/if}

  <footer class="panel-foot">
    <button class="btn" onclick={onClose}>取消</button>
    <button class="btn primary" disabled={!canSave} onclick={save}>
      {saving ? '保存中…' : '保存'}
    </button>
  </footer>
</div>

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.28);
    z-index: var(--z-modal);
    animation: item-enter var(--dur-fast) var(--ease-spring) both;
    cursor: default;
  }

  .panel {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    z-index: calc(var(--z-modal) + 1);
    width: min(520px, calc(100vw - 48px));
    max-height: calc(100vh - 64px);
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
    padding: var(--space-6);
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-lg);
    animation: popover-in var(--dur-slow) var(--ease-spring) both;
  }

  .panel-title {
    font-size: var(--text-heading);
    font-weight: var(--weight-semibold);
  }

  /* ================= 字段 ================= */
  .field {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }

  .label {
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .input {
    width: 100%;
    padding: var(--space-2) var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-bg);
    color: var(--color-text);
    font-size: var(--text-item);
    font-family: inherit;
    /* 表单里必须允许选中文本，否则用户无法复制粘贴 */
    user-select: text;
  }

  .input:focus {
    outline: none;
    border-color: var(--color-accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-accent) 18%, transparent);
  }

  .textarea {
    resize: vertical;
    line-height: var(--leading-normal);
  }

  .datetime {
    display: flex;
    gap: var(--space-2);
  }

  .time-input {
    flex: 0 0 120px;
  }

  /* ================= 时间类型选择 ================= */
  .kinds {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: var(--space-2);
  }

  .kind {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: var(--space-3);
    text-align: left;
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    transition:
      border-color var(--dur-fast) var(--ease-spring),
      background var(--dur-fast) var(--ease-spring);
  }

  .kind:hover {
    border-color: var(--color-accent);
  }

  .kind.on {
    background: var(--color-bg-selected);
    border-color: var(--color-accent);
  }

  .kind-label {
    font-size: var(--text-item);
    color: var(--color-text);
  }

  .kind.on .kind-label {
    color: var(--color-accent);
    font-weight: var(--weight-medium);
  }

  .kind-hint {
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
    line-height: var(--leading-normal);
  }

  /* ================= 分段控件 ================= */
  .segmented {
    display: flex;
    padding: 2px;
    border-radius: var(--radius-md);
    background: var(--color-bg-hover);
    align-self: flex-start;
  }

  .seg {
    padding: var(--space-1) var(--space-4);
    border-radius: var(--radius-sm);
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .seg.on {
    background: var(--color-bg-elevated);
    color: var(--color-text);
    font-weight: var(--weight-medium);
    box-shadow: var(--shadow-sm);
  }

  /* ================= 底部 ================= */
  .panel-foot {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
    padding-top: var(--space-2);
  }

  .btn {
    padding: var(--space-2) var(--space-5);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-bg);
    color: var(--color-text);
    font-size: var(--text-item);
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .btn:hover:not(:disabled) {
    background: var(--color-bg-hover);
  }

  .btn.primary {
    background: var(--color-accent);
    border-color: var(--color-accent);
    color: #fff;
    font-weight: var(--weight-medium);
  }

  .btn.primary:hover:not(:disabled) {
    background: var(--color-accent-hover);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .note {
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
    line-height: var(--leading-normal);
  }

  .error {
    font-size: var(--text-caption);
    color: var(--color-danger);
  }
</style>
