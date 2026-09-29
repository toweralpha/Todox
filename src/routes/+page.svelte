<script lang="ts">
  /**
   * Todox 应用外壳与视图路由。
   *
   * 阶段五：日历、统计、设置三个视图已接入，任务可编辑。
   *
   * 关于"路由"的实现选择：没有引入任何路由库。这是一个单窗口桌面应用，
   * 视图切换只是一个状态变量 —— 引入路由库会带来 URL 管理、历史栈、
   * 预加载等一整套与桌面应用无关的概念，以及额外的体积。
   */

  import { onMount } from 'svelte';
  import TaskRow from '$lib/components/TaskRow.svelte';
  import MissedBanner from '$lib/components/MissedBanner.svelte';
  import TaskEditor from '$lib/components/TaskEditor.svelte';
  import CalendarView from '$lib/views/Calendar.svelte';
  import StatsView from '$lib/views/Stats.svelte';
  import SettingsView from '$lib/views/Settings.svelte';
  import { taskStore, type ParsePreview, type Task } from '$lib/stores/tasks.svelte';
  import { settingsStore } from '$lib/stores/settings.svelte';
  import { dayDiff, parseTime } from '$lib/utils/datetime';

  type ViewId = 'today' | 'inbox' | 'upcoming' | 'all' | 'calendar' | 'stats' | 'settings';

  interface NavItem {
    id: ViewId;
    label: string;
    /** 24x24 视口下的图标路径。线条图标更贴近苹果侧边栏的克制感。 */
    icon: string;
  }

  const primaryNav: NavItem[] = [
    {
      id: 'today',
      label: '今天',
      icon: 'M8 2v2M16 2v2M3.5 9h17M5 4.5h14a1.5 1.5 0 0 1 1.5 1.5v13A1.5 1.5 0 0 1 19 20.5H5A1.5 1.5 0 0 1 3.5 19V6A1.5 1.5 0 0 1 5 4.5Z'
    },
    {
      id: 'inbox',
      label: '收件箱',
      icon: 'M3.5 13.5h4l1.2 2.2h6.6l1.2-2.2h4M3.5 13.5 6 5.2A1.5 1.5 0 0 1 7.4 4.2h9.2A1.5 1.5 0 0 1 18 5.2l2.5 8.3v5A1.5 1.5 0 0 1 19 20h-14a1.5 1.5 0 0 1-1.5-1.5v-5Z'
    },
    {
      id: 'upcoming',
      label: '即将到期',
      icon: 'M12 7v5l3.2 2M12 21a9 9 0 1 0 0-18 9 9 0 0 0 0 18Z'
    },
    {
      id: 'all',
      label: '全部',
      icon: 'M4 6.5h16M4 12h16M4 17.5h10'
    }
  ];

  const secondaryNav: NavItem[] = [
    {
      id: 'calendar',
      label: '日历',
      icon: 'M4 8.5h16M8 3v3M16 3v3M5.5 5h13A1.5 1.5 0 0 1 20 6.5v12A1.5 1.5 0 0 1 18.5 20h-13A1.5 1.5 0 0 1 4 18.5v-12A1.5 1.5 0 0 1 5.5 5Z'
    },
    {
      id: 'stats',
      label: '完成统计',
      icon: 'M6 20V11M12 20V4M18 20v-5'
    }
  ];

  let activeView = $state<ViewId>('today');
  let draft = $state('');
  let submitting = $state(false);
  let preview = $state<ParsePreview | null>(null);

  /** 正在编辑的任务。为 null 表示编辑面板关闭。 */
  let editing = $state<Task | null>(null);

  /** 进入设置页之前所在的视图，用于"返回"。 */
  let viewBeforeSettings = $state<ViewId>('today');

  /** 最近一次删除的任务，用于提供撤销入口。 */
  let lastDeleted = $state<{ id: string; title: string } | null>(null);

  /** 快速添加输入框的引用，供全局快捷键聚焦。 */
  let quickAddInput = $state<HTMLInputElement | null>(null);

  const allNav = [...primaryNav, ...secondaryNav];
  const activeLabel = $derived(
    activeView === 'settings'
      ? '设置'
      : (allNav.find((n) => n.id === activeView)?.label ?? '今天')
  );

  const todayLabel = $derived(
    new Intl.DateTimeFormat('zh-CN', {
      month: 'long',
      day: 'numeric',
      weekday: 'long'
    }).format(new Date())
  );

  onMount(() => {
    void taskStore.load();

    // 监听后端的"唤出窗口"事件（全局快捷键 / 托盘点击）。
    // 后端只负责把窗口显示出来，聚焦输入框需要前端配合。
    let unlisten: (() => void) | undefined;
    void (async () => {
      try {
        const { listen } = await import('@tauri-apps/api/event');
        unlisten = await listen('todox://focus-quick-add', () => {
          activeView = 'today';
          quickAddInput?.focus();
        });
      } catch {
        // 事件系统不可用时静默降级：窗口仍会被唤出，只是不聚焦输入框
      }
    })();

    return () => unlisten?.();
  });

  /**
   * 输入内容变化时刷新解析预览。
   *
   * 用 180ms 防抖：每敲一个字都跨进程调用一次是浪费，而 180ms 短到
   * 用户在停顿看完标签之前就已经更新完了。
   */
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
    }, 180);
  }

  async function quickAdd(event: SubmitEvent) {
    event.preventDefault();
    const text = draft.trim();
    if (!text || submitting) return;

    submitting = true;
    const ok = await taskStore.createFromText(text);
    submitting = false;

    if (ok) {
      draft = '';
      preview = null;
    }
  }

  async function handleToggle(task: Task) {
    await taskStore.toggleComplete(task);
  }

  async function handleDelete(id: string) {
    const task = taskStore.tasks.find((t) => t.id === id);
    const ok = await taskStore.remove(id);
    if (ok && task) {
      lastDeleted = { id: task.id, title: task.title };
    }
  }

  async function undoDelete() {
    if (!lastDeleted) return;
    const ok = await taskStore.restore(lastDeleted.id);
    if (ok) lastDeleted = null;
  }

  function openSettings() {
    viewBeforeSettings = activeView === 'settings' ? 'today' : activeView;
    activeView = 'settings';
  }

  /** 任务的时间基准：截止型看 deadline_at，其余看 due_at。 */
  function anchorOf(task: Task): string | null {
    return task.time_kind === 'before_deadline' ? task.deadline_at : task.due_at;
  }

  /**
   * 按当前视图过滤任务。
   *
   * "今天"包含**已逾期**的任务，而不是只留当天。理由是逾期任务若不出现在
   * 今天视图里，用户很容易彻底忘记它 —— 这正是待办应用最该避免的失败模式。
   */
  const visibleTasks = $derived.by(() => {
    const tasks = taskStore.tasks;

    switch (activeView) {
      case 'today':
        return tasks.filter((t) => {
          const anchor = parseTime(anchorOf(t));
          if (!anchor) return false;
          return dayDiff(anchor) <= 0;
        });

      case 'inbox':
        return tasks.filter((t) => !t.due_at && !t.deadline_at);

      case 'upcoming': {
        const future = tasks.filter((t) => {
          const anchor = parseTime(anchorOf(t));
          return anchor !== null && dayDiff(anchor) > 0;
        });
        // 数据库已排过序，但视图过滤后顺序可能被打乱，这里重新排一次
        return future.sort((a, b) => {
          const ta = parseTime(anchorOf(a))?.getTime() ?? 0;
          const tb = parseTime(anchorOf(b))?.getTime() ?? 0;
          return ta - tb;
        });
      }

      case 'all':
      default:
        return tasks;
    }
  });

  /** 列表型视图（今天 / 收件箱 / 即将到期 / 全部）才显示快速添加与列表。 */
  const isList = $derived(
    activeView === 'today' ||
      activeView === 'inbox' ||
      activeView === 'upcoming' ||
      activeView === 'all'
  );
</script>

<div class="shell">
  <!-- ================= 侧边栏 ================= -->
  <aside class="sidebar">
    <header class="titlebar" data-tauri-drag-region>
      <div class="brand">
        <span class="brand-mark" aria-hidden="true">
          <svg viewBox="0 0 24 24" width="15" height="15" fill="none">
            <path
              d="m5 12.5 4.5 4.5L19 7.5"
              stroke="currentColor"
              stroke-width="2.5"
              stroke-linecap="round"
              stroke-linejoin="round"
            />
          </svg>
        </span>
        <span class="brand-name">Todox</span>
      </div>
    </header>

    <nav class="nav scroll-area" aria-label="主导航">
      <ul>
        {#each primaryNav as item (item.id)}
          <li>
            <button
              class="nav-item"
              class:active={activeView === item.id}
              aria-current={activeView === item.id ? 'page' : undefined}
              onclick={() => (activeView = item.id)}
            >
              <svg class="nav-icon" viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
                <path
                  d={item.icon}
                  stroke="currentColor"
                  stroke-width="1.7"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                />
              </svg>
              <span class="nav-label">{item.label}</span>
              {#if item.id === 'today' && taskStore.unfinished > 0}
                <span class="nav-badge tnum">{taskStore.unfinished}</span>
              {/if}
            </button>
          </li>
        {/each}
      </ul>

      <div class="nav-divider" role="presentation"></div>

      <ul>
        {#each secondaryNav as item (item.id)}
          <li>
            <button
              class="nav-item"
              class:active={activeView === item.id}
              aria-current={activeView === item.id ? 'page' : undefined}
              onclick={() => (activeView = item.id)}
            >
              <svg class="nav-icon" viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
                <path
                  d={item.icon}
                  stroke="currentColor"
                  stroke-width="1.7"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                />
              </svg>
              <span class="nav-label">{item.label}</span>
            </button>
          </li>
        {/each}
      </ul>
    </nav>

    <footer class="sidebar-footer">
      <button
        class="nav-item"
        class:active={activeView === 'settings'}
        onclick={openSettings}
      >
        <svg class="nav-icon" viewBox="0 0 24 24" width="18" height="18" fill="none" aria-hidden="true">
          <circle cx="12" cy="12" r="3.2" stroke="currentColor" stroke-width="1.7" />
          <path
            d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-2.87 1.2v.17a2 2 0 1 1-4 0v-.09A1.7 1.7 0 0 0 8 19.4a1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 3.5 14a1.7 1.7 0 0 0-1.55-1H1.8a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 3.5 7.4a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 8 3.5h.09A1.7 1.7 0 0 0 9 1.95V1.8a2 2 0 1 1 4 0v.09A1.7 1.7 0 0 0 15 3.5a1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 8v.09a1.7 1.7 0 0 0 1.55 1h.15a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.61 1Z"
            stroke="currentColor"
            stroke-width="1.5"
            stroke-linecap="round"
            stroke-linejoin="round"
          />
        </svg>
        <span class="nav-label">设置</span>
      </button>
    </footer>
  </aside>

  <!-- ================= 内容区 ================= -->
  <main class="content scroll-area">
    {#if activeView === 'calendar'}
      <CalendarView onToggle={handleToggle} onDelete={handleDelete} />
    {:else if activeView === 'stats'}
      <StatsView />
    {:else if activeView === 'settings'}
      <SettingsView onBack={() => (activeView = viewBeforeSettings)} />
    {:else}
      <div class="content-inner">
        <header class="page-head">
          <p class="page-date">{todayLabel}</p>
          <h1 class="page-title">{activeLabel}</h1>
        </header>

        <form class="quick-add" onsubmit={quickAdd}>
          <span class="quick-add-plus" aria-hidden="true">
            <svg viewBox="0 0 24 24" width="16" height="16" fill="none">
              <path d="M12 5v14M5 12h14" stroke="currentColor" stroke-width="2" stroke-linecap="round" />
            </svg>
          </span>
          <input
            class="quick-add-input"
            bind:this={quickAddInput}
            bind:value={draft}
            oninput={onDraftInput}
            placeholder="添加任务，例如「每周一早上9点 提交周报」"
            aria-label="快速添加任务"
            autocomplete="off"
            spellcheck="false"
            disabled={submitting}
          />
          {#if draft.trim()}
            <kbd class="quick-add-hint">回车</kbd>
          {/if}
        </form>

        <!-- 解析预览：必须在保存前让用户看到时间是怎么被理解的 -->
        {#if preview && draft.trim()}
          <div class="preview" role="status">
            <span class="preview-label">识别为</span>
            {#if preview.has_time}
              {#if preview.recurrence_label}
                <span class="chip chip-time">{preview.recurrence_label}</span>
              {:else if preview.time_label}
                <span class="chip chip-time">{preview.time_label}</span>
              {/if}
            {:else}
              <span class="chip chip-none">未识别到时间 · 存入收件箱</span>
            {/if}
            {#if preview.title && preview.title.trim() !== draft.trim()}
              <span class="preview-title">标题：{preview.title}</span>
            {/if}
          </div>
        {/if}

        <MissedBanner />

        {#if taskStore.error}
          <div class="banner banner-error" role="alert">
            <span>{taskStore.error}</span>
            <button class="banner-action" onclick={() => taskStore.clearError()}>知道了</button>
          </div>
        {/if}

        {#if lastDeleted}
          <div class="banner banner-undo" role="status">
            <span>已删除「{lastDeleted.title}」</span>
            <button class="banner-action" onclick={undoDelete}>撤销</button>
          </div>
        {/if}

        {#if taskStore.loading && taskStore.tasks.length === 0}
          <p class="hint">正在读取…</p>
        {:else if visibleTasks.length === 0}
          <div class="empty">
            <div class="empty-art" aria-hidden="true">
              <svg viewBox="0 0 24 24" width="26" height="26" fill="none">
                <path
                  d="m5 12.5 4.5 4.5L19 7.5"
                  stroke="currentColor"
                  stroke-width="1.8"
                  stroke-linecap="round"
                  stroke-linejoin="round"
                />
              </svg>
            </div>
            <p class="empty-title">{activeView === 'today' ? '今天没有要做的事' : '这里还是空的'}</p>
            <p class="empty-desc">
              在上面输入框写下要做的事，时间可以随口说 ——
              比如「明天下午3点 交房租」「每周一早上9点 提交周报」。
            </p>
          </div>
        {:else}
          <ul class="task-list">
            {#each visibleTasks as task (task.id)}
              <div class="row-wrap">
                <TaskRow
                  {task}
                  onDelete={handleDelete}
                  onToggle={handleToggle}
                  justCompleted={taskStore.justCompleted === task.id}
                />
                <button
                  class="edit-btn"
                  onclick={() => (editing = task)}
                  aria-label="编辑任务"
                  title="编辑（可改成截止型任务以启用分级提醒）"
                >
                  编辑
                </button>
              </div>
            {/each}
          </ul>
        {/if}
      </div>
    {/if}
  </main>
</div>

{#if editing}
  <TaskEditor task={editing} onClose={() => (editing = null)} />
{/if}

<style>
  .shell {
    display: grid;
    grid-template-columns: var(--sidebar-width) 1fr;
    height: 100vh;
    background: var(--color-bg);
  }

  /* ================= 侧边栏 ================= */
  .sidebar {
    display: flex;
    flex-direction: column;
    background: var(--color-bg-sidebar);
    border-right: 1px solid var(--color-border);
    min-width: 0;
  }

  .titlebar {
    height: var(--titlebar-height);
    display: flex;
    align-items: center;
    padding: 0 var(--space-4);
    flex-shrink: 0;
  }

  .brand {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }

  .brand-mark {
    display: grid;
    place-items: center;
    width: 20px;
    height: 20px;
    border-radius: var(--radius-sm);
    background: var(--color-accent);
    color: #fff;
    flex-shrink: 0;
  }

  .brand-name {
    font-size: var(--text-caption);
    font-weight: var(--weight-semibold);
    letter-spacing: 0.01em;
    color: var(--color-text-secondary);
  }

  .nav {
    flex: 1;
    padding: var(--space-2) var(--space-2) 0;
    min-height: 0;
  }

  .nav-divider {
    height: 1px;
    margin: var(--space-2) var(--space-3);
    background: var(--color-border);
  }

  .nav-item {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    width: 100%;
    min-height: var(--row-height-min);
    padding: 0 var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--text-item);
    color: var(--color-text);
    text-align: left;
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .nav-item:hover {
    background: var(--color-bg-hover);
  }

  .nav-item.active {
    background: var(--color-bg-selected);
    color: var(--color-accent);
    font-weight: var(--weight-medium);
  }

  .nav-icon {
    flex-shrink: 0;
    display: block;
  }

  /* 颜色只表达状态：未选中时图标一律中性 */
  .nav-item:not(.active) .nav-icon {
    color: var(--color-text-secondary);
  }

  .nav-item.active .nav-icon {
    color: var(--color-accent);
  }

  .nav-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .nav-badge {
    flex-shrink: 0;
    min-width: 18px;
    height: 18px;
    padding: 0 5px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-full);
    background: var(--color-accent);
    color: #fff;
    font-size: var(--text-mini);
    font-weight: var(--weight-medium);
  }

  .sidebar-footer {
    padding: var(--space-2);
    border-top: 1px solid var(--color-border);
    flex-shrink: 0;
  }

  /* ================= 内容区 ================= */
  .content {
    min-width: 0;
    display: flex;
    justify-content: center;
  }

  .content-inner {
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

  .page-date {
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .page-title {
    font-size: var(--text-title);
    line-height: var(--leading-tight);
    font-weight: var(--weight-semibold);
    letter-spacing: -0.02em;
  }

  /* ================= 快速添加 ================= */
  .quick-add {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 0 var(--space-4);
    height: 48px;
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-sm);
    transition:
      border-color var(--dur-fast) var(--ease-spring),
      box-shadow var(--dur-fast) var(--ease-spring);
  }

  .quick-add:focus-within {
    border-color: var(--color-accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-accent) 18%, transparent);
  }

  .quick-add-plus {
    display: grid;
    place-items: center;
    color: var(--color-accent);
    flex-shrink: 0;
  }

  .quick-add-input {
    flex: 1;
    min-width: 0;
    border: none;
    outline: none;
    background: transparent;
    font-size: var(--text-body);
    color: var(--color-text);
  }

  .quick-add-input::placeholder {
    color: var(--color-text-tertiary);
  }

  .quick-add-hint {
    flex-shrink: 0;
    font-family: var(--font-sans);
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    padding: 2px 6px;
  }

  /* ================= 解析预览 ================= */
  .preview {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-top: calc(-1 * var(--space-3));
    padding: 0 var(--space-1);
    font-size: var(--text-caption);
    animation: item-enter var(--dur-fast) var(--ease-spring) both;
  }

  .preview-label {
    color: var(--color-text-tertiary);
  }

  .chip {
    padding: 2px 9px;
    border-radius: var(--radius-full);
    font-weight: var(--weight-medium);
  }

  .chip-time {
    background: var(--color-bg-selected);
    color: var(--color-accent);
  }

  .chip-none {
    background: var(--color-bg-hover);
    color: var(--color-text-secondary);
    font-weight: var(--weight-regular);
  }

  .preview-title {
    color: var(--color-text-secondary);
  }

  /* ================= 提示条 ================= */
  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-card);
    font-size: var(--text-caption);
    animation: item-enter var(--dur-base) var(--ease-spring) both;
  }

  .banner-error {
    background: color-mix(in srgb, var(--color-danger) 12%, transparent);
    color: var(--color-danger);
  }

  .banner-undo {
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    color: var(--color-text-secondary);
  }

  .banner-action {
    flex-shrink: 0;
    font-weight: var(--weight-medium);
    color: var(--color-accent);
  }

  .banner-action:hover {
    text-decoration: underline;
  }

  /* ================= 列表 ================= */
  .task-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
  }

  /* 编辑按钮悬浮时才出现，减少视觉噪音；
     但用 :focus-within 保证键盘用户仍能到达它。 */
  .row-wrap {
    position: relative;
    display: flex;
    align-items: center;
  }

  .row-wrap > :global(li) {
    flex: 1;
    min-width: 0;
  }

  .edit-btn {
    flex-shrink: 0;
    padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--text-mini);
    color: var(--color-accent);
    opacity: 0;
    transition: opacity var(--dur-fast) var(--ease-spring);
  }

  .row-wrap:hover .edit-btn,
  .edit-btn:focus-visible {
    opacity: 1;
  }

  .edit-btn:hover {
    background: var(--color-bg-selected);
  }

  .hint {
    padding: var(--space-8) 0;
    text-align: center;
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  /* ================= 空状态 ================= */
  .empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    text-align: center;
    gap: var(--space-2);
    padding: var(--space-12) var(--space-6);
  }

  .empty-art {
    display: grid;
    place-items: center;
    width: 56px;
    height: 56px;
    margin-bottom: var(--space-2);
    border-radius: var(--radius-lg);
    background: var(--color-bg-hover);
    color: var(--color-text-tertiary);
  }

  .empty-title {
    font-size: var(--text-heading);
    font-weight: var(--weight-medium);
  }

  .empty-desc {
    max-width: 36ch;
    font-size: var(--text-caption);
    line-height: var(--leading-normal);
    color: var(--color-text-secondary);
  }
</style>
