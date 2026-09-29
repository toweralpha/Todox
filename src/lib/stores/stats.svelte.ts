/**
 * 完成统计状态管理。
 *
 * 与 Rust 侧 `repo/stats_repo.rs` 的类型手工对齐。
 */

import { invoke } from '@tauri-apps/api/core';
import { toMessage } from './tasks.svelte';

/** 对应 Rust 的 `StatsOverview`。 */
export interface StatsOverview {
  unfinished: number;
  completed: number;
  total_completions: number;
  today_completions: number;
  last_7_days_completions: number;
  last_30_days_completions: number;
  overdue: number;
}

/** 对应 Rust 的 `DailyCount`。 */
export interface DailyCount {
  date: string;
  count: number;
}

/** 对应 Rust 的 `TaskStreak`。 */
export interface TaskStreak {
  task_id: string;
  task_title: string;
  total: number;
  first_at: string | null;
  last_at: string | null;
}

class StatsStore {
  overview = $state<StatsOverview | null>(null);
  daily = $state<DailyCount[]>([]);
  byTask = $state<TaskStreak[]>([]);
  loading = $state(false);
  error = $state<string | null>(null);

  /** 最近 N 天的完成数里的最大值。柱状图用它来定标高度。 */
  maxDaily = $derived(
    this.daily.reduce((max, d) => (d.count > max ? d.count : max), 0)
  );

  async load(days = 30): Promise<void> {
    this.loading = true;
    this.error = null;
    try {
      // 三个查询互不依赖，并发发出以减少等待时间
      const [overview, daily, byTask] = await Promise.all([
        invoke<StatsOverview>('stats_overview'),
        invoke<DailyCount[]>('stats_daily', { days }),
        invoke<TaskStreak[]>('stats_by_task', { limit: 10 })
      ]);
      this.overview = overview;
      this.daily = daily;
      this.byTask = byTask;
    } catch (err) {
      this.error = toMessage(err);
    } finally {
      this.loading = false;
    }
  }

  clearError(): void {
    this.error = null;
  }
}

export const statsStore = new StatsStore();
