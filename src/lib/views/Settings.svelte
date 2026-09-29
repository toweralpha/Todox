<script lang="ts">
  /**
   * 设置页。
   *
   * 关于保存时机：**每个控件改动后立即保存**，没有"保存"按钮。
   * 桌面应用的设置页通常是即时生效的，多一个保存按钮反而会让用户不确定
   * "我改了但没点保存，到底生效了没有"。开机自启是例外 —— 它直接操作系统
   * 设置，因此单独处理并回读真实状态。
   */

  import { open, save, message, ask } from '@tauri-apps/plugin-dialog';
  import { readTextFile, writeTextFile, stat } from '@tauri-apps/plugin-fs';
  import { invoke } from '@tauri-apps/api/core';
  import { settingsStore, offsetLabel, type Theme } from '$lib/stores/settings.svelte';
  import { taskStore, toMessage } from '$lib/stores/tasks.svelte';

  interface Props {
    onBack?: () => void;
  }

  let { onBack }: Props = $props();

  /** 导入导出的进行状态，用于禁用按钮避免重复点击。 */
  let busy = $state(false);
  let ioMessage = $state<string | null>(null);
  let ioIsError = $state(false);

  const s = $derived(settingsStore.settings);

  // ===================== 提醒档位的编辑 =====================

  /**
   * 可选的提醒提前量，**单位是秒**（与后端一致）。
   *
   * 用固定选项而不是让用户输入任意数字：提醒档位的合理取值是有限的几种，
   * "提前 37 分钟"这种需求几乎不存在，而自由输入很容易得到一个无意义的数字。
   */
  const OFFSET_CHOICES = [
    { seconds: -604800, label: '提前 7 天' },
    { seconds: -172800, label: '提前 2 天' },
    { seconds: -86400, label: '提前 1 天' },
    { seconds: -43200, label: '提前 12 小时' },
    { seconds: -14400, label: '提前 4 小时' },
    { seconds: -3600, label: '提前 1 小时' },
    { seconds: -1800, label: '提前 30 分钟' },
    { seconds: -600, label: '提前 10 分钟' },
    { seconds: 0, label: '到点' }
  ];

  /** 勾选/取消某个提醒档位。 */
  async function toggleOffset(kind: 'deadline' | 'point', seconds: number) {
    if (!s) return;
    const key =
      kind === 'deadline' ? 'deadline_offsets_seconds' : 'point_offsets_seconds';
    const current = s[key];
    let next: number[];

    if (current.includes(seconds)) {
      // 至少保留一个档位：全部取消等于"永不提醒"，那几乎肯定是误操作，
      // 而且用户不会意识到自己关掉了所有提醒。
      if (current.length === 1) {
        ioMessage = '至少需要保留一个提醒时间';
        ioIsError = true;
        return;
      }
      next = current.filter((m) => m !== seconds);
    } else {
      next = [...current, seconds].sort((a, b) => a - b);
    }

    await settingsStore.patch({ [key]: next } as never);
  }

  // ===================== 导入导出 =====================

  async function doExport() {
    busy = true;
    ioMessage = null;
    try {
      // 先让用户选路径，再生成数据 —— 反过来会让用户等两次
      const path = await save({
        title: '导出 Todox 数据',
        defaultPath: `todox-backup-${new Date().toISOString().slice(0, 10)}.json`,
        filters: [{ name: 'JSON', extensions: ['json'] }]
      });
      if (!path) {
        busy = false;
        return;
      }

      const envelope = await invoke<unknown>('export_data');
      const text = JSON.stringify(envelope, null, 2);
      await writeTextFile(path, text);

      ioMessage = `已导出到 ${path}`;
      ioIsError = false;
    } catch (err) {
      ioMessage = toMessage(err);
      ioIsError = true;
    } finally {
      busy = false;
    }
  }

  async function doImport() {
    busy = true;
    ioMessage = null;
    try {
      const selected = await open({
        title: '选择要导入的 Todox 备份',
        multiple: false,
        filters: [{ name: 'JSON', extensions: ['json'] }]
      });
      if (!selected || typeof selected !== 'string') {
        busy = false;
        return;
      }

      // 读文件之前先看大小。
      //
      // 整个文件会被读进 WebView 内存、JSON.parse 成对象、再整包经 IPC
      // 交给 Rust（约 3–4 倍放大）。误选一个几百 MB 的 JSON 会让 WebView
      // 直接崩掉，而崩溃信息对用户毫无意义。这里提前拦住并给出明确原因。
      const info = await stat(selected);
      const MAX_IMPORT_BYTES = 100 * 1024 * 1024;
      if (info.size > MAX_IMPORT_BYTES) {
        ioMessage = `这个文件有 ${(info.size / 1024 / 1024).toFixed(1)} MB，超过 100 MB 的导入上限。请确认是否选错了文件。`;
        ioIsError = true;
        busy = false;
        return;
      }

      const text = await readTextFile(selected);
      const envelope = JSON.parse(text);

      // 导入是破坏性操作，必须让用户明确选择方式。
      //
      // 早期实现用 `confirm`（只有两个选项），于是「取消」被当成"合并" ——
      // 用户想**中止**这个危险操作，却反而走进了会改动数据的路径。
      // 这里改用两步询问来实现真正的三路选择：中止 / 清空并复原 / 仅合并。
      //
      // 放在 JSON 解析**之后**：文件本身不是合法备份时，先告诉用户这个事实，
      // 而不是让他先做一个无意义的选择。
      const restore = await ask(
        '要用这份备份「完整复原」吗？\n\n' +
          '· 选择「是」= 清空当前全部数据，然后用备份替换（真正的恢复）\n' +
          '· 选择「否」= 只按 ID 合并，保留当前已有的数据',
        { title: '导入方式', kind: 'warning', okLabel: '清空并复原', cancelLabel: '仅合并' }
      );

      // 第二步给出真正的"中止"。只有用户明确选了方式才继续。
      const proceed = await ask(
        restore
          ? '即将**清空当前全部数据**并用备份替换。\n\n这个操作无法撤销。确定继续吗？'
          : '将按 ID 合并备份中的记录。\n\n当前已有的数据会保留（同 ID 的以备份为准）。确定继续吗？',
        {
          title: '确认导入',
          kind: 'warning',
          okLabel: '开始导入',
          cancelLabel: '中止'
        }
      );

      if (!proceed) {
        ioMessage = '已取消导入，数据未做任何改动。';
        ioIsError = false;
        busy = false;
        return;
      }

      const summary = await invoke<{
        total: number;
        ignored_columns?: Record<string, string[]>;
      }>('import_data', {
        envelope,
        replaceExisting: restore
      });

      // 导入后必须重新加载：界面上的列表还是旧数据
      await taskStore.load();
      await settingsStore.load();

      // 被丢弃的未知列必须告知用户 —— 否则就是静默的数据丢失
      const ignored = summary.ignored_columns ?? {};
      const ignoredNames = Object.entries(ignored).flatMap(([, cols]) => cols);
      ioMessage =
        `导入完成，共 ${summary.total} 条记录` +
        (ignoredNames.length > 0
          ? `。有 ${ignoredNames.length} 个字段本程序不认识（可能来自更新版本的备份），已跳过：${ignoredNames.slice(0, 5).join('、')}`
          : '');
      ioIsError = ignoredNames.length > 0;
    } catch (err) {
      // JSON.parse 失败会抛 SyntaxError，它的 message 是英文的、
      // 对用户没有意义，因此换成明确的中文提示。
      const msg =
        err instanceof SyntaxError
          ? '这个文件不是有效的 JSON，可能已损坏或不是 Todox 备份'
          : toMessage(err);
      ioMessage = msg;
      ioIsError = true;
    } finally {
      busy = false;
    }
  }

  async function showDataLocation() {
    await message(
      '待办数据保存在系统标准的应用数据目录：\n\n%APPDATA%\\com.tower.todox\\todox.db\n\n' +
        '同目录下的 -wal 与 -shm 文件是 SQLite 的正常产物。\n' +
        '如需迁移或备份，请使用上面的「导出数据」。',
      { title: '数据存放位置', kind: 'info' }
    );
  }
</script>

<div class="page">
  <header class="page-head">
    <div class="head-row">
      <h1 class="page-title">设置</h1>
      {#if onBack}
        <button class="ghost-btn" onclick={onBack}>返回</button>
      {/if}
    </div>
    <p class="page-sub">改动会立即生效并保存</p>
  </header>

  {#if settingsStore.error}
    <div class="banner banner-error" role="alert">
      <span>{settingsStore.error}</span>
      <button class="banner-action" onclick={() => settingsStore.clearError()}>知道了</button>
    </div>
  {/if}

  {#if !s}
    <p class="hint">正在读取设置…</p>
  {:else}
    <!-- ============ 通知 ============ -->
    <section class="card">
      <h2 class="card-title">通知</h2>

      <label class="row">
        <span class="row-label">
          开启提醒通知
          <span class="row-desc">关闭后仍会记录提醒，只是不弹通知</span>
        </span>
        <input
          type="checkbox"
          class="switch"
          checked={s.notifications_enabled}
          onchange={(e) =>
            settingsStore.patch({
              notifications_enabled: (e.currentTarget as HTMLInputElement).checked
            })}
        />
      </label>

      <label class="row">
        <span class="row-label">
          勿扰时段
          <span class="row-desc">该时段内不弹通知，避免半夜被打扰</span>
        </span>
        <input
          type="checkbox"
          class="switch"
          checked={s.quiet_hours_enabled}
          onchange={(e) =>
            settingsStore.patch({
              quiet_hours_enabled: (e.currentTarget as HTMLInputElement).checked
            })}
        />
      </label>

      {#if s.quiet_hours_enabled}
        <div class="row row-indent">
          <span class="row-label">静默时间</span>
          <div class="inline-fields">
            <select
              class="select"
              value={s.quiet_hours_start}
              onchange={(e) =>
                settingsStore.patch({
                  quiet_hours_start: Number((e.currentTarget as HTMLSelectElement).value)
                })}
            >
              {#each Array.from({ length: 24 }, (_, i) => i) as h}
                <option value={h}>{String(h).padStart(2, '0')}:00</option>
              {/each}
            </select>
            <span class="dash">至</span>
            <select
              class="select"
              value={s.quiet_hours_end}
              onchange={(e) =>
                settingsStore.patch({
                  quiet_hours_end: Number((e.currentTarget as HTMLSelectElement).value)
                })}
            >
              {#each Array.from({ length: 24 }, (_, i) => i) as h}
                <option value={h}>{String(h).padStart(2, '0')}:00</option>
              {/each}
            </select>
          </div>
        </div>
        {#if s.quiet_hours_start === s.quiet_hours_end}
          <p class="warn-note">起止时间相同，表示全天静默。</p>
        {/if}
      {/if}

      <div class="row">
        <span class="row-label">
          「稍后提醒」时长
          <span class="row-desc">点稍后提醒时推迟多久</span>
        </span>
        <select
          class="select"
          value={s.snooze_minutes}
          onchange={(e) =>
            settingsStore.patch({
              snooze_minutes: Number((e.currentTarget as HTMLSelectElement).value)
            })}
        >
          {#each [5, 10, 15, 30, 60] as m}
            <option value={m}>{m} 分钟</option>
          {/each}
        </select>
      </div>
    </section>

    <!-- ============ 提醒时间 ============ -->
    <section class="card">
      <h2 class="card-title">提醒时间</h2>

      <div class="field-group">
        <p class="group-label">
          截止型任务
          <span class="row-desc">有明确截止时间、需要提前做的任务</span>
        </p>
        <div class="chips">
          {#each OFFSET_CHOICES as choice (choice.seconds)}
            <button
              class="chip"
              class:on={s.deadline_offsets_seconds.includes(choice.seconds)}
              onclick={() => toggleOffset('deadline', choice.seconds)}
            >
              {choice.label}
            </button>
          {/each}
        </div>
        <p class="current-note">
          当前：{s.deadline_offsets_seconds.map(offsetLabel).join(' · ') || '（无）'}
        </p>
      </div>

      <div class="field-group">
        <p class="group-label">
          时间点 / 重复任务
          <span class="row-desc">「3 点开会」这类任务，默认只在到点提醒</span>
        </p>
        <div class="chips">
          {#each OFFSET_CHOICES as choice (choice.seconds)}
            <button
              class="chip"
              class:on={s.point_offsets_seconds.includes(choice.seconds)}
              onclick={() => toggleOffset('point', choice.seconds)}
            >
              {choice.label}
            </button>
          {/each}
        </div>
        <p class="current-note">
          当前：{s.point_offsets_seconds.map(offsetLabel).join(' · ') || '（无）'}
        </p>
      </div>

      <div class="row">
        <span class="row-label">
          全天任务提醒时刻
          <span class="row-desc">只写了日期、没写具体时间的任务</span>
        </span>
        <div class="inline-fields">
          <select
            class="select"
            value={s.all_day_hour}
            onchange={(e) =>
              settingsStore.patch({
                all_day_hour: Number((e.currentTarget as HTMLSelectElement).value)
              })}
          >
            {#each Array.from({ length: 24 }, (_, i) => i) as h}
              <option value={h}>{String(h).padStart(2, '0')}</option>
            {/each}
          </select>
          <span class="dash">:</span>
          <select
            class="select"
            value={s.all_day_minute}
            onchange={(e) =>
              settingsStore.patch({
                all_day_minute: Number((e.currentTarget as HTMLSelectElement).value)
              })}
          >
            {#each [0, 15, 30, 45] as m}
              <option value={m}>{String(m).padStart(2, '0')}</option>
            {/each}
          </select>
        </div>
      </div>
    </section>

    <!-- ============ 外观与行为 ============ -->
    <section class="card">
      <h2 class="card-title">外观与行为</h2>

      <div class="row">
        <span class="row-label">主题</span>
        <div class="segmented">
          {#each [['system', '跟随系统'], ['light', '浅色'], ['dark', '深色']] as [value, label] (value)}
            <button
              class="seg"
              class:on={s.theme === value}
              onclick={() => settingsStore.patch({ theme: value as Theme })}
            >
              {label}
            </button>
          {/each}
        </div>
      </div>

      <label class="row">
        <span class="row-label">
          关闭窗口时保留在托盘
          <span class="row-desc">关闭后仍会按时提醒；从托盘菜单可真正退出</span>
        </span>
        <input
          type="checkbox"
          class="switch"
          checked={s.close_to_tray}
          onchange={(e) =>
            settingsStore.patch({
              close_to_tray: (e.currentTarget as HTMLInputElement).checked
            })}
        />
      </label>

      <label class="row">
        <span class="row-label">
          开机时自动启动
          <span class="row-desc">状态直接读取自系统，而非本地记录</span>
        </span>
        <input
          type="checkbox"
          class="switch"
          checked={settingsStore.autostartEnabled}
          onchange={(e) => settingsStore.setAutostart((e.currentTarget as HTMLInputElement).checked)}
        />
      </label>
    </section>

    <!-- ============ 数据 ============ -->
    <section class="card">
      <h2 class="card-title">数据</h2>

      <div class="row">
        <span class="row-label">
          备份与恢复
          <span class="row-desc">导出为可读的 JSON 文件，可用于迁移或留档</span>
        </span>
        <div class="btn-group">
          <button class="btn" disabled={busy} onclick={doExport}>导出</button>
          <button class="btn" disabled={busy} onclick={doImport}>导入</button>
        </div>
      </div>

      <div class="row">
        <span class="row-label">数据文件位置</span>
        <button class="btn" onclick={showDataLocation}>查看</button>
      </div>

      {#if ioMessage}
        <p class="io-note" class:error={ioIsError}>{ioMessage}</p>
      {/if}
    </section>
  {/if}
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
    align-items: baseline;
    justify-content: space-between;
    gap: var(--space-4);
  }

  .page-title {
    font-size: var(--text-title);
    line-height: var(--leading-tight);
    font-weight: var(--weight-semibold);
    letter-spacing: -0.02em;
  }

  .page-sub {
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .ghost-btn {
    font-size: var(--text-caption);
    color: var(--color-accent);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
  }

  .ghost-btn:hover {
    background: var(--color-bg-hover);
  }

  /* ================= 卡片 ================= */
  .card {
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
    padding: var(--space-5) var(--space-5) var(--space-4);
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-sm);
  }

  .card-title {
    font-size: var(--text-caption);
    font-weight: var(--weight-semibold);
    color: var(--color-text-secondary);
    text-transform: none;
    letter-spacing: 0.02em;
    padding-bottom: var(--space-2);
  }

  /* ================= 行 ================= */
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
    min-height: var(--row-height-min);
    padding: var(--space-2) 0;
  }

  .row:not(:last-child) {
    border-bottom: 1px solid var(--color-border);
  }

  .row-indent {
    padding-left: var(--space-4);
  }

  .row-label {
    display: flex;
    flex-direction: column;
    gap: 2px;
    font-size: var(--text-item);
    min-width: 0;
  }

  .row-desc {
    font-size: var(--text-mini);
    color: var(--color-text-secondary);
    line-height: var(--leading-normal);
  }

  /* ================= 控件 ================= */
  /* 开关用原生 checkbox 加样式覆盖，而不是自绘 div：
     原生元素自带键盘操作、焦点环与屏幕阅读器语义，自绘要重新实现一遍
     而且极易漏掉无障碍支持。 */
  .switch {
    flex-shrink: 0;
    appearance: none;
    width: 44px;
    height: 26px;
    border-radius: var(--radius-full);
    background: var(--color-border-strong);
    position: relative;
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .switch::after {
    content: '';
    position: absolute;
    top: 3px;
    left: 3px;
    width: 20px;
    height: 20px;
    border-radius: var(--radius-full);
    background: #fff;
    box-shadow: var(--shadow-sm);
    transition: transform var(--dur-fast) var(--ease-spring);
  }

  .switch:checked {
    background: var(--color-success);
  }

  .switch:checked::after {
    transform: translateX(18px);
  }

  .select {
    padding: var(--space-1) var(--space-2);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    background: var(--color-bg);
    color: var(--color-text);
    font-size: var(--text-caption);
    cursor: pointer;
  }

  .inline-fields {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-shrink: 0;
  }

  .dash {
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .btn {
    padding: var(--space-2) var(--space-4);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-bg);
    color: var(--color-text);
    font-size: var(--text-caption);
    font-weight: var(--weight-medium);
    cursor: pointer;
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .btn:hover:not(:disabled) {
    background: var(--color-bg-hover);
  }

  .btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }

  .btn-group {
    display: flex;
    gap: var(--space-2);
    flex-shrink: 0;
  }

  /* 分段控件 */
  .segmented {
    display: flex;
    padding: 2px;
    border-radius: var(--radius-md);
    background: var(--color-bg-hover);
    flex-shrink: 0;
  }

  .seg {
    padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
    transition:
      background var(--dur-fast) var(--ease-spring),
      color var(--dur-fast) var(--ease-spring);
  }

  .seg.on {
    background: var(--color-bg-elevated);
    color: var(--color-text);
    font-weight: var(--weight-medium);
    box-shadow: var(--shadow-sm);
  }

  /* ================= 提醒档位 ================= */
  .field-group {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) 0;
  }

  .field-group:not(:last-child) {
    border-bottom: 1px solid var(--color-border);
  }

  .group-label {
    display: flex;
    flex-direction: column;
    gap: 2px;
    font-size: var(--text-item);
  }

  .chips {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
  }

  .chip {
    padding: var(--space-1) var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-full);
    font-size: var(--text-mini);
    color: var(--color-text-secondary);
    cursor: pointer;
    transition:
      background var(--dur-fast) var(--ease-spring),
      color var(--dur-fast) var(--ease-spring),
      border-color var(--dur-fast) var(--ease-spring);
  }

  .chip:hover {
    border-color: var(--color-accent);
    color: var(--color-accent);
  }

  /* 选中用主色 —— 颜色只表达状态 */
  .chip.on {
    background: var(--color-bg-selected);
    border-color: var(--color-accent);
    color: var(--color-accent);
    font-weight: var(--weight-medium);
  }

  .current-note {
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  /* ================= 提示 ================= */
  .banner {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-card);
    font-size: var(--text-caption);
  }

  .banner-error {
    background: color-mix(in srgb, var(--color-danger) 12%, transparent);
    color: var(--color-danger);
  }

  .banner-action {
    flex-shrink: 0;
    font-weight: var(--weight-medium);
    color: var(--color-accent);
  }

  .io-note {
    padding-top: var(--space-2);
    font-size: var(--text-caption);
    color: var(--color-success);
    overflow-wrap: anywhere;
  }

  .io-note.error {
    color: var(--color-danger);
  }

  .warn-note {
    font-size: var(--text-mini);
    color: var(--color-warning);
    padding-bottom: var(--space-2);
  }

  .hint {
    padding: var(--space-8) 0;
    text-align: center;
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }
</style>


