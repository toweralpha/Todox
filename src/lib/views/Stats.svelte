<script lang="ts">
  /**
   * 完成统计页。
   *
   * 柱状图用纯 CSS 实现（每个柱子一个 div，高度用百分比）。
   * 不引入图表库：一个 30 根柱子的条形图用不上几十 KB 的绘图库，
   * 而本项目的核心指标之一就是体积与内存。
   */

  import { onMount } from 'svelte';
  import { statsStore } from '$lib/stores/stats.svelte';

  let range = $state(30);

  onMount(() => {
    void statsStore.load(range);
  });

  async function setRange(days: number) {
    range = days;
    await statsStore.load(days);
  }

  /** 柱子的高度百分比。最大值为 0 时全部显示为最小值，避免除零。 */
  function heightPct(count: number): number {
    const max = statsStore.maxDaily;
    if (max <= 0) return 0;
    // 给最小非零值一个可见的高度：否则"完成 1 件"和"没完成"看起来一样
    return count === 0 ? 0 : Math.max(8, Math.round((count / max) * 100));
  }

  /** 只显示月-日，完整日期太长会把横轴挤满。 */
  function shortDate(iso: string): string {
    const parts = iso.split('-');
    return parts.length === 3 ? `${Number(parts[1])}/${Number(parts[2])}` : iso;
  }

  /** 柱状图横轴标签的显示间隔。柱子多时隔几个显示一个，避免文字重叠。 */
  const labelStride = $derived(range <= 7 ? 1 : range <= 14 ? 2 : 5);

  function spanText(first: string | null, last: string | null): string {
    if (!first || !last) return '';
    const d = (s: string) => s.slice(5, 10).replace('-', '/');
    return d(first) === d(last) ? d(first) : `${d(first)} – ${d(last)}`;
  }
</script>

<div class="page">
  <header class="page-head">
    <h1 class="page-title">完成统计</h1>
    <p class="page-sub">这里只统计「完成记录」，重复任务每完成一轮都算一次</p>
  </header>

  {#if statsStore.error}
    <div class="banner banner-error" role="alert">{statsStore.error}</div>
  {/if}

  {#if !statsStore.overview}
    <p class="hint">正在统计…</p>
  {:else}
    <!-- ============ 概览数字 ============ -->
    <section class="metrics">
      <div class="metric">
        <span class="metric-value tnum">{statsStore.overview.unfinished}</span>
        <span class="metric-label">未完成</span>
      </div>
      <div class="metric">
        <span class="metric-value tnum">{statsStore.overview.today_completions}</span>
        <span class="metric-label">今天完成</span>
      </div>
      <div class="metric">
        <span class="metric-value tnum">{statsStore.overview.last_7_days_completions}</span>
        <span class="metric-label">近 7 天</span>
      </div>
      <div class="metric">
        <span class="metric-value tnum">{statsStore.overview.last_30_days_completions}</span>
        <span class="metric-label">近 30 天</span>
      </div>
      <!-- 逾期用警示色：它是这里唯一需要用户采取行动的数字 -->
      <div class="metric" class:alert={statsStore.overview.overdue > 0}>
        <span class="metric-value tnum">{statsStore.overview.overdue}</span>
        <span class="metric-label">已逾期</span>
      </div>
    </section>

    <!-- ============ 每日完成柱状图 ============ -->
    <section class="card">
      <div class="card-head">
        <h2 class="card-title">每日完成</h2>
        <div class="segmented">
          {#each [7, 30, 90] as d (d)}
            <button class="seg" class:on={range === d} onclick={() => setRange(d)}>
              {d} 天
            </button>
          {/each}
        </div>
      </div>

      {#if statsStore.maxDaily === 0}
        <p class="empty-line">这段时间还没有完成记录。</p>
      {:else}
        <div class="chart" role="img" aria-label="每日完成数量柱状图">
          {#each statsStore.daily as day, i (day.date)}
            <div class="bar-slot">
              <div class="bar-wrap">
                {#if day.count > 0}
                  <span class="bar-value tnum">{day.count}</span>
                {/if}
                <div
                  class="bar"
                  style:height="{heightPct(day.count)}%"
                  title="{day.date}：完成 {day.count} 件"
                ></div>
              </div>
              <span class="bar-label">
                {i % labelStride === 0 ? shortDate(day.date) : ''}
              </span>
            </div>
          {/each}
        </div>
      {/if}
    </section>

    <!-- ============ 按任务排行 ============ -->
    <section class="card">
      <h2 class="card-title">完成最多的任务</h2>

      {#if statsStore.byTask.length === 0}
        <p class="empty-line">还没有任何完成记录。</p>
      {:else}
        <ul class="rank-list">
          {#each statsStore.byTask as item, i (item.task_id)}
            <li class="rank-row">
              <span class="rank-index tnum">{i + 1}</span>
              <span class="rank-body">
                <span class="rank-title">{item.task_title}</span>
                <span class="rank-span">{spanText(item.first_at, item.last_at)}</span>
              </span>
              <span class="rank-total tnum">{item.total} 次</span>
            </li>
          {/each}
        </ul>
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

  /* ================= 概览 ================= */
  .metrics {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(104px, 1fr));
    gap: var(--space-3);
  }

  .metric {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 2px;
    padding: var(--space-4) var(--space-2);
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-sm);
  }

  .metric-value {
    font-size: 26px;
    font-weight: var(--weight-semibold);
    line-height: 1.1;
    letter-spacing: -0.02em;
  }

  .metric-label {
    font-size: var(--text-mini);
    color: var(--color-text-secondary);
  }

  /* 逾期用危险色 —— 颜色只表达状态 */
  .metric.alert .metric-value {
    color: var(--color-danger);
  }

  /* ================= 卡片 ================= */
  .card {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    padding: var(--space-5);
    background: var(--color-bg-elevated);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-card);
    box-shadow: var(--shadow-sm);
  }

  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
    flex-wrap: wrap;
  }

  .card-title {
    font-size: var(--text-caption);
    font-weight: var(--weight-semibold);
    color: var(--color-text-secondary);
    letter-spacing: 0.02em;
  }

  .segmented {
    display: flex;
    padding: 2px;
    border-radius: var(--radius-md);
    background: var(--color-bg-hover);
  }

  .seg {
    padding: var(--space-1) var(--space-3);
    border-radius: var(--radius-sm);
    font-size: var(--text-mini);
    color: var(--color-text-secondary);
    transition: background var(--dur-fast) var(--ease-spring);
  }

  .seg.on {
    background: var(--color-bg-elevated);
    color: var(--color-text);
    font-weight: var(--weight-medium);
    box-shadow: var(--shadow-sm);
  }

  /* ================= 柱状图 ================= */
  .chart {
    display: flex;
    align-items: flex-end;
    gap: 2px;
    /* 固定高度而不是自适应：图形高度固定才能让"多"与"少"的视觉反差稳定，
       否则换个窗口大小读出的结论就变了 */
    height: 140px;
    padding-top: var(--space-4);
  }

  .bar-slot {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    justify-content: flex-end;
    height: 100%;
  }

  .bar-wrap {
    position: relative;
    flex: 1;
    display: flex;
    align-items: flex-end;
    justify-content: center;
  }

  .bar {
    width: 100%;
    max-width: 18px;
    min-height: 0;
    border-radius: 3px 3px 2px 2px;
    background: var(--color-accent);
    transition: height var(--dur-slow) var(--ease-spring);
  }

  .bar-value {
    position: absolute;
    top: -2px;
    font-size: 9px;
    color: var(--color-text-tertiary);
    line-height: 1;
  }

  .bar-label {
    flex-shrink: 0;
    height: 14px;
    text-align: center;
    font-size: 9px;
    color: var(--color-text-tertiary);
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }

  /* ================= 排行 ================= */
  .rank-list {
    display: flex;
    flex-direction: column;
  }

  .rank-row {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    min-height: 40px;
    padding: var(--space-1) 0;
  }

  .rank-row:not(:last-child) {
    border-bottom: 1px solid var(--color-border);
  }

  .rank-index {
    flex-shrink: 0;
    width: 18px;
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
    text-align: right;
  }

  .rank-body {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .rank-title {
    font-size: var(--text-item);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .rank-span {
    font-size: var(--text-mini);
    color: var(--color-text-tertiary);
  }

  .rank-total {
    flex-shrink: 0;
    font-size: var(--text-caption);
    font-weight: var(--weight-medium);
    color: var(--color-text-secondary);
  }

  /* ================= 其它 ================= */
  .empty-line {
    padding: var(--space-6) 0;
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .hint {
    padding: var(--space-8) 0;
    text-align: center;
    font-size: var(--text-caption);
    color: var(--color-text-secondary);
  }

  .banner-error {
    padding: var(--space-3) var(--space-4);
    border-radius: var(--radius-card);
    background: color-mix(in srgb, var(--color-danger) 12%, transparent);
    color: var(--color-danger);
    font-size: var(--text-caption);
  }
</style>
