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
  import { invoke } from '@tauri-apps/api/core';
  import { taskStore, type ParsePreview, type Task } from '$lib/stores/tasks.svelte';
  import {
    formatCountdown,
    formatTimeLabel,
    parseTime
  } from '$lib/utils/datetime';

  /**
   * 隐藏悬浮窗。
   *
   * # 为什么走 Tauri 命令而不是 `getCurrentWindow().hide()`
   *
   * 前端直接调窗口 API 会受 capability 限制：`core:default` **不包含**
   * 任何改变窗口状态的命令，缺 `core:window:allow-hide` 时 Tauri 会拒绝调用。
   *
   * 本项目已经因此出过一次故障：关闭按钮和 Esc 都"点了没反应"，
   * 而真正的原因（一行清晰的权限错误）被 `catch` 吞掉了。
   * 这类故障体验极差、用户又完全无法自查，因此改成**后端命令** ——
   * 命令不受 capability 限制，从根上排除这一整类静默失败。
   *
   * 失败时写 console.error 而不是静默：即使有了这条更可靠的路，
   * 也不该再让错误无声无息地消失。
   */
  async function hide() {
    try {
      await invoke('hide_float_window');
    } catch (e) {
      console.error('隐藏悬浮窗失败：', e);
    }
  }

  /**
   * 拖动悬浮窗。
   *
   * 无边框窗口必须自己提供拖拽。这里**不用** `data-tauri-drag-region`：
   * 那个属性由 Tauri 注入的脚本处理，最终仍会调用需要 capability 授权的
   * 窗口命令；改成后端命令后行为更可控，也能自己决定哪些子元素不参与拖动。
   */
  function onHeaderMouseDown(e: MouseEvent) {
    if (e.button !== 0) return;
    // 按钮/输入框等交互元素要保留自己的行为，不能被当成拖拽区
    const target = e.target as HTMLElement | null;
    if (target?.closest('button, input, a, select, textarea')) return;
    void invoke('start_float_drag').catch((err) => {
      console.error('拖动悬浮窗失败：', err);
    });
  }

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

    // 这里**刻意不注册"失焦自动隐藏"**。
    //
    // 曾经的实现在窗口失去焦点时自动隐藏窗口，有两个问题：
    //   1. 它会静默丢弃用户正在输入的内容 —— 只是想切到别处复制一段文字，
    //      回来发现草稿没了，而没有任何提示；
    //   2. 关闭方式变得不可预期（有时点别处就没了，有时又不会），
    //      而用户真正想要的是**明确的关闭方式**：Esc、右上角按钮、
    //      或再按一次快捷键。
    //
    // 悬浮窗只在用户主动唤出时出现、本身很小，不做自动隐藏不会妨碍使用；
    // 换来的是"它一定在，直到我说关"这种可预期的行为。
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
  <!-- 可拖动区域。
       不用 data-tauri-drag-region：那条路要经过 capability 授权，
       缺权限时窗口拖不动且静默失败。这里自己处理 mousedown，
       并排除内部的按钮（否则按关闭按钮会变成拖窗口）。

       role="group"：这一行既充当窗口拖拽把手，又是品牌标识与关闭按钮的容器。
       给它一个角色是因为"带 mousedown 的静态元素"对辅助技术不可见。

       svelte-ignore：这条警告在这里是误报。窗口拖拽是纯指针操作，
       平台层面就不存在"用键盘拖动窗口"这回事（原生标题栏同样没有）。
       真正需要键盘可达的是内部的关闭按钮，它本身就是 <button>，天然可聚焦，
       而且已经有 aria-label。为了一个不存在键盘等价物的交互去加 tabindex
       只会让 Tab 顺序里多一个什么都不做的停靠点，反而更差。 -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <header
    class="head"
    role="group"
    aria-label="悬浮窗标题栏"
    onmousedown={onHeaderMouseDown}
  >
    <span class="brand">
      <span class="dot" aria-hidden="true"></span>
      Todox
    </span>
    <button
      class="close"
      type="button"
      onclick={hide}
      aria-label="关闭悬浮窗"
      title="关闭（Esc）"
    >
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
