<script lang="ts">
  /**
   * 悬浮窗（float 窗口）的快速待办面板。
   *
   * 定位：让用户「不离开手上的事」就能看一眼接下来要做什么、随手记一条。
   * 因此它刻意**不是主窗口的缩小版** —— 没有侧边栏、没有视图切换、
   * 没有设置入口。那些都需要"进入应用"，而这个窗口的意义恰恰在于不必进入。
   *
   * 交互约定：
   *   - 顶部区域可拖动（无边框窗口必须自己提供拖拽区）
   *   - 失焦自动隐藏，避免长期遮挡其它窗口
   *   - Esc 隐藏
   *   - 输入框回车即保存并清空，可连续录入
   */

  import { onMount } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import { taskStore, type ParsePreview, type Task } from '$lib/stores/tasks.svelte';
  import {
    formatCountdown,
    formatTimeLabel,
    parseTime
  } from '$lib/utils/datetime';

  /** 最多显示几条。空间有限，塞太多就失去了"简洁"的意义。 */
  const MAX_ITEMS = 6;

  let draft = $state('');
  let preview = $state<ParsePreview | null>(null);
  let submitting = $state(false);
  let inputEl = $state<HTMLInputElement | null>(null);

  onMount(() => {
    // 注意 onMount 的回调**不能是 async 函数**：那样它返回的是一个 Promise，
    // 而 Svelte 期望的是清理函数。异步初始化必须在内部自行发起。
    void (async () => {
      await taskStore.load();
      // 打开即聚焦输入框：用户唤出悬浮窗的目的通常就是"记一件事"
      inputEl?.focus();
    })();

    // 失焦自动隐藏。一个始终浮在最上层的面板会从助手变成干扰。
    let unlisten: (() => void) | undefined;
    void (async () => {
      try {
        const win = getCurrentWindow();
        unlisten = await win.onFocusChanged(({ payload: focused }) => {
          if (!focused) void win.hide();
        });
      } catch {
        // 事件不可用时窗口仍可手动关闭，不影响主要功能
      }
    })();

    return () => unlisten?.();
  });

  function anchorOf(task: Task): string | null {
    return task.time_kind === 'before_deadline' ? task.deadline_at : task.due_at;
  }

  /**
   * 最近要做的几件事：未完成、有时间、按时间升序。
   *
   * **包含已逾期的**并排在最前 —— 逾期任务若被"接下来的事"过滤掉，用户就会
   * 彻底忘掉它，而这正是待办应用最该避免的失败模式。
   */
  const upcoming = $derived.by(() =>
    taskStore.tasks
      .filter((t) => !t.is_completed && anchorOf(t) !== null)
      .sort((a, b) => {
        const ta = parseTime(anchorOf(a))?.getTime() ?? 0;
        const tb = parseTime(anchorOf(b))?.getTime() ?? 0;
        return ta - tb;
      })
      .slice(0, MAX_ITEMS)
  );

  let previewTimer: ReturnType<typeof setTimeout> | undefined;

  function onDraftInput() {
    const text = draft.trim();
    clearTimeout(previewTimer);
    if (!text) {
      preview = null;
      return;
    }
    previewTimer = setTimeout(async () => {
      preview = await taskStore.preview(text);
    }, 150);
  }

  async function quickAdd(event: SubmitEvent) {
    event.preventDefault();
    const text = draft.trim();
    if (!text || submitting) return;

    submitting = true;
    // 走与主窗口**完全相同**的解析与建任务链路。
    // 悬浮窗不该有自己的一套时间解析逻辑，否则两边行为会不一致 ——
    // 那类不一致极难排查，用户只会觉得"有时候好使有时候不好使"。
    const ok = await taskStore.createFromText(text);
    submitting = false;

    if (ok) {
      draft = '';
      preview = null;
      inputEl?.focus(); // 保持焦点以便连续录入
    }
  }

  async function toggle(task: Task) {
    await taskStore.toggleComplete(task);
  }

  async function hide() {
    try {
      await getCurrentWindow().hide();
    } catch {
      /* 隐藏失败无需打扰用户 */
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Escape') void hide();
  }

  /** 时间基准是否已过去。 */
  function isOverdue(task: Task): boolean {
    const a = parseTime(anchorOf(task));
    return a !== null && a.getTime() < Date.now();
  }
</script>

<svelte:window onkeydown={onKeydown} />

<div class="panel">
  <!-- 可拖动区域。无边框窗口必须自己提供，否则用户无法移动它。 -->
  <header class="head" data-tauri-drag-region>
    <span class="brand" data-tauri-drag-region>
      <span class="dot" aria-hidden="true"></span>
      Todox
    </span>
    <button class="close" onclick={hide} aria-label="隐藏悬浮窗" title="隐藏（Esc）">
      <svg viewBox="0 0 24 24" width="13" height="13" fill="none">
        <path
          d="M6 6l12 12M18 6L6 18"
          stroke="currentColor"
          stroke-width="2"
          stroke-linecap="round"
        />
      </svg>
    </button>
  </header>

  <!-- 快速添加 -->
  <form class="add" onsubmit={quickAdd}>
    <svg class="add-icon" viewBox="0 0 24 24" width="15" height="15" fill="none" aria-hidden="true">
      <path d="M12 5v14M5 12h14" stroke="currentColor" stroke-width="2" stroke-linecap="round" />
    </svg>
    <input
      bind:this={inputEl}
      class="add-input"
      bind:value={draft}
      oninput={onDraftInput}
      placeholder="记一件事，例如「20分钟后 打电话」"
      aria-label="快速添加任务"
      autocomplete="off"
      spellcheck="false"
      disabled={submitting}
    />
  </form>

  <!-- 解析预览：与主窗口一致，保存前让用户看到时间是怎么被理解的 -->
  {#if preview && draft.trim()}
    <div class="preview">
      {#if preview.has_time}
        <span class="chip on">
          {preview.recurrence_label ?? preview.time_label ?? '已识别时间'}
        </span>
      {:else}
        <span class="chip">未识别到时间 · 存入收件箱</span>
      {/if}
    </div>
  {/if}

  {#if taskStore.error}
    <p class="error">{taskStore.error}</p>
  {/if}

  <!-- 接下来要做的事 -->
  <div class="list-wrap scroll-area">
    {#if upcoming.length === 0}
      <p class="empty">接下来没有安排。</p>
    {:else}
      <ul class="list">
        {#each upcoming as task (task.id)}
          {@const overdue = isOverdue(task)}
          <li class="row" class:overdue>
            <button
              class="check"
              onclick={() => toggle(task)}
              aria-label="标记完成"
              title={task.time_kind === 'recurring' ? '完成这一轮' : '标记完成'}
            >
              <svg viewBox="0 0 24 24" width="10" height="10" fill="none" aria-hidden="true">
                <path
                  d="m5 12.5 4.5 4.5L19 7.5"
                  stroke="currentColor"
                  stroke-width="3.2"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                />
              </svg>
            </button>

            <span class="title">{task.title}</span>

            <span class="when tnum">
              {#if overdue}
                已逾期
              {:else if task.time_kind === 'before_deadline'}
                <!-- 截止型任务显示倒计时，它对"还来不来得及"更直观 -->
                {formatCountdown(task.deadline_at)}
              {:else}
                {formatTimeLabel(anchorOf(task))}
              {/if}
            </span>
          </li>
        {/each}
      </ul>
    {/if}
  </div>

  <footer class="foot">
    <span>回车添加 · Esc 隐藏</span>
    {#if taskStore.unfinished > 0}
      <span class="count tnum">{taskStore.unfinished} 件未完成</span>
    {/if}
  </footer>
</div>

<style>
  /* ===== 悬浮面板本体 =====
     窗口是 transparent + decorations:false，因此圆角、边框与阴影都由这一层负责。
     透明窗口里若不自己画背景，用户会看到桌面直接穿透进来。 */
  .panel {
    display: flex;
    flex-direction: column;
    height: calc(100vh - 16px);
    margin: 8px;
    /* 留出 8px margin 是为了让阴影有扩散空间（窗口尺寸已按此预留） */
    background: color-mix(in srgb, var(--color-bg-elevated) 94%, transparent);
    backdrop-filter: blur(24px) saturate(1.4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
  }

  /* ===== 顶部 ===== */
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    height: 36px;
    padding: 0 var(--space-2) 0 var(--space-4);
    flex-shrink: 0;
    /* 拖拽区给出 cursor 提示，否则用户不知道这里能拖 */
    cursor: grab;
  }

  .head:active {
    cursor: grabbing;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-mini);
    font-weight: var(--weight-semibold);
    letter-spacing: 0.02em;
    color: var(--color-text-secondary);
  }

  .dot {
    width: 6px;
    height: 6px;
    border-radius: var(--radius-full);
    background: var(--color-accent);
  }

  .close {
    width: 22px;
    height: 22px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-sm);
    color: var(--color-text-tertiary);
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .close:hover {
    background: var(--color-bg-hover);
    color: var(--color-text);
  }

  /* ===== 快速添加 ===== */
  .add {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    margin: 0 var(--space-3);
    padding: 0 var(--space-3);
    height: 38px;
    flex-shrink: 0;
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    transition:
      border-color var(--dur-fast) var(--ease-spring),
      box-shadow var(--dur-fast) var(--ease-spring);
  }

  .add:focus-within {
    border-color: var(--color-accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-accent) 16%, transparent);
  }

  .add-icon {
    flex-shrink: 0;
    color: var(--color-accent);
  }

  .add-input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: transparent;
    font-size: var(--text-caption);
    color: var(--color-text);
  }

  .add-input::placeholder {
    color: var(--color-text-tertiary);
  }

  /* ===== 解析预览 ===== */
  .preview {
    padding: var(--space-2) var(--space-3) 0;
    flex-shrink: 0;
  }

  .chip {
    display: inline-block;
    padding: 2px 8px;
    border-radius: var(--radius-full);
    background: var(--color-bg-hover);
    color: var(--color-text-secondary);
    font-size: var(--text-mini);
  }

  .chip.on {
    background: var(--color-bg-selected);
    color: var(--color-accent);
    font-weight: var(--weight-medium);
  }

  /* ===== 列表 ===== */
  .list-wrap {
    flex: 1;
    min-height: 0;
    padding: var(--space-3) var(--space-2) var(--space-2);
  }

  .list {
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-2);
    border-radius: var(--radius-sm);
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .row:hover {
    background: var(--color-bg-hover);
  }

  .check {
    flex-shrink: 0;
    width: 16px;
    height: 16px;
    display: grid;
    place-items: center;
    border: 1.5px solid var(--color-border-strong);
    border-radius: var(--radius-full);
    color: transparent;
    transition:
      border-color var(--dur-fast) var(--ease-spring),
      background var(--dur-fast) var(--ease-spring);
  }

  .check:hover {
    border-color: var(--color-success);
    background: color-mix(in srgb, var(--color-success) 18%, transparent);
  }

  .title {
    flex: 1;
    min-width: 0;
    font-size: var(--text-caption);
    color: var(--color-text);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .when {
    flex-shrink: 0;
    font-size: var(--text-mini);
    color: var(--color-text-secondary);
    white-space: nowrap;
  }

  /* 逾期用危险色 —— 颜色只表达状态 */
  .row.overdue .when {
    color: var(--color-danger);
    font-weight: var(--weight-medium);
  }

  .empty {
    padding: var(--space-8) var(--space-4);
    text-align: center;
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  .error {
    margin: 0 var(--space-3);
    font-size: var(--text-mini);
    color: var(--color-danger);
  }

  /* ===== 底部 ===== */
  .foot {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-2) var(--space-4);
    flex-shrink: 0;
    border-top: 1px solid var(--color-border);
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  .count {
    color: var(--color-text-secondary);
  }
</style>
