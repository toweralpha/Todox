<script lang="ts">
  /**
   * 错过提醒的提示条。
   *
   * 这是"防漏机制"面向用户的那一半：调度器负责把错过的提醒落库，
   * 这个组件负责让用户**看见**它们。
   *
   * 设计上刻意不用弹窗：错过的提醒是"信息"而不是"需要立刻决策的事"，
   * 用模态弹窗打断用户会让这个功能变得讨人厌，而它的目的恰恰是提醒，
   * 不是骚扰。
   */

  import { taskStore } from '$lib/stores/tasks.svelte';
  import { formatTimeLabel } from '$lib/utils/datetime';

  /** 展开/收起详细列表。默认收起，只显示条数与最近一条。 */
  let expanded = $state(false);

  const count = $derived(taskStore.missed.length);
  const latest = $derived(taskStore.missed[taskStore.missed.length - 1]);
</script>

{#if count > 0}
  <div class="banner" role="status">
    <div class="banner-main">
      <span class="badge tnum">{count}</span>
      <div class="text">
        <p class="title">
          你错过了 {count} 个提醒
        </p>
        {#if !expanded && latest}
          <p class="sub">
            最近一个：{latest.task_title} · {formatTimeLabel(latest.scheduled_at)}
          </p>
        {/if}
      </div>
      <button class="link" onclick={() => (expanded = !expanded)}>
        {expanded ? '收起' : '查看'}
      </button>
      <button class="link" onclick={() => taskStore.acknowledgeAllMissed()}>
        全部知道了
      </button>
    </div>

    {#if expanded}
      <ul class="list">
        {#each [...taskStore.missed].reverse() as m (m.id)}
          <li class="item">
            <span class="item-title">{m.task_title}</span>
            <span class="item-time tnum">{formatTimeLabel(m.scheduled_at)}</span>
            <button
              class="item-dismiss"
              onclick={() => taskStore.acknowledgeMissed(m.id)}
              aria-label="知道了"
            >
              知道了
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </div>
{/if}

<style>
  .banner {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    padding: var(--space-3) var(--space-4);
    background: color-mix(in srgb, var(--color-warning) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--color-warning) 32%, transparent);
    border-radius: var(--radius-card);
    animation: item-enter var(--dur-base) var(--ease-spring) both;
  }

  .banner-main {
    display: flex;
    align-items: center;
    gap: var(--space-3);
  }

  .badge {
    flex-shrink: 0;
    min-width: 22px;
    height: 22px;
    padding: 0 6px;
    display: grid;
    place-items: center;
    border-radius: var(--radius-full);
    background: var(--color-warning);
    color: #fff;
    font-size: var(--text-mini);
    font-weight: var(--weight-semibold);
  }

  .text {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 1px;
  }

  .title {
    font-size: var(--text-caption);
    font-weight: var(--weight-medium);
    color: var(--color-text);
  }

  .sub {
    font-size: var(--text-mini);
    color: var(--color-text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .link {
    flex-shrink: 0;
    font-size: var(--text-caption);
    font-weight: var(--weight-medium);
    color: var(--color-accent);
  }

  .link:hover {
    text-decoration: underline;
  }

  /* ================= 详细列表 ================= */
  .list {
    display: flex;
    flex-direction: column;
    max-height: 200px;
    overflow-y: auto;
    padding-left: calc(22px + var(--space-3));
  }

  .item {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) 0;
    font-size: var(--text-caption);
  }

  .item:not(:last-child) {
    border-bottom: 1px solid color-mix(in srgb, var(--color-warning) 20%, transparent);
  }

  .item-title {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--color-text);
  }

  .item-time {
    flex-shrink: 0;
    color: var(--color-text-secondary);
  }

  .item-dismiss {
    flex-shrink: 0;
    font-size: var(--text-mini);
    color: var(--color-accent);
  }

  .item-dismiss:hover {
    text-decoration: underline;
  }
</style>
