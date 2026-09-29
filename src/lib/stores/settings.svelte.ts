/**
 * 设置状态管理。
 *
 * 与 Rust 侧 `repo/settings_repo.rs` 的 `AppSettings` 手工对齐。
 * 改动 Rust 侧字段时**必须同步改这里**。
 */

import { invoke } from '@tauri-apps/api/core';
import { enable, disable, isEnabled } from '@tauri-apps/plugin-autostart';
import { toMessage } from './tasks.svelte';

export type Theme = 'system' | 'light' | 'dark';

/** 对应 Rust 的 `AppSettings`。字段名必须与 serde 的命名一致。 */
export interface AppSettings {
  notifications_enabled: boolean;
  quiet_hours_start: number;
  quiet_hours_end: number;
  quiet_hours_enabled: boolean;
  /**
   * 截止型任务的提醒档位。
   *
   * **单位是秒**（与 Rust 侧一致）：负值为提前，-86400 表示提前一天。
   * 字段名里带 seconds 就是为了避免"分钟还是秒"的歧义 ——
   * 曾经因为单位不一致导致「10 秒后提醒我」被截断成 0。
   */
  deadline_offsets_seconds: number[];
  /** 时间点 / 重复型任务的提醒档位（单位：秒）。 */
  point_offsets_seconds: number[];
  all_day_hour: number;
  all_day_minute: number;
  close_to_tray: boolean;
  theme: Theme;
  snooze_minutes: number;
}

/**
 * 把秒偏移渲染成中文标签，例如 -86400 → 「提前 1 天」、0 → 「到点」。
 *
 * 放在前端而不是后端：后端只需要存数字，展示格式属于界面职责。
 */
export function offsetLabel(seconds: number): string {
  if (seconds === 0) return '到点';
  const abs = Math.abs(seconds);
  const prefix = seconds < 0 ? '提前' : '延后';

  if (abs % 86400 === 0) {
    return `${prefix} ${abs / 86400} 天`;
  }
  if (abs % 3600 === 0) {
    return `${prefix} ${abs / 3600} 小时`;
  }
  if (abs % 60 === 0) {
    return `${prefix} ${abs / 60} 分钟`;
  }
  return `${prefix} ${abs} 秒`;
}

class SettingsStore {
  settings = $state<AppSettings | null>(null);
  loading = $state(false);
  error = $state<string | null>(null);
  /** 开机自启的实际系统状态。它与设置页里的开关是两回事，需单独读。 */
  autostartEnabled = $state(false);

  async load(): Promise<void> {
    this.loading = true;
    this.error = null;
    try {
      this.settings = await invoke<AppSettings>('get_settings');
    } catch (err) {
      this.error = toMessage(err);
    } finally {
      this.loading = false;
    }
    await this.refreshAutostart();
  }

  /**
   * 读取开机自启的真实状态。
   *
   * 刻意从系统读而不是从我们的设置表读：用户可能在任务管理器的"启动"页里
   * 手动关掉它，此时设置表里还记着"已开启"，界面就会撒谎。
   */
  async refreshAutostart(): Promise<void> {
    try {
      this.autostartEnabled = await isEnabled();
    } catch {
      // 插件不可用（例如非打包环境）时静默降级为"关闭"，
      // 而不是在界面上弹一个用户无法处理的错误。
      this.autostartEnabled = false;
    }
  }

  /** 保存设置。保存成功后由调度器负责重算提醒。 */
  async save(next: AppSettings): Promise<boolean> {
    this.error = null;
    try {
      await invoke('save_settings', { settings: next });
      this.settings = next;
      this.applyTheme(next.theme);
      return true;
    } catch (err) {
      this.error = toMessage(err);
      return false;
    }
  }

  /** 只改某几项。 */
  async patch(changes: Partial<AppSettings>): Promise<boolean> {
    if (!this.settings) return false;
    return this.save({ ...this.settings, ...changes });
  }

  async setAutostart(on: boolean): Promise<boolean> {
    this.error = null;
    try {
      if (on) {
        await enable();
      } else {
        await disable();
      }
      await this.refreshAutostart();
      return true;
    } catch (err) {
      this.error = toMessage(err);
      await this.refreshAutostart();
      return false;
    }
  }

  /**
   * 把主题应用到根元素。
   *
   * 只有 'light' / 'dark' 会设置 `data-theme`；'system' 时移除该属性，
   * 让 CSS 的 `prefers-color-scheme` 媒体查询接管。这样"跟随系统"是真正的
   * 跟随 —— 用户在系统设置里切换主题时应用会立即跟着变，不需要我们监听。
   */
  applyTheme(theme: Theme): void {
    const root = document.documentElement;
    if (theme === 'system') {
      root.removeAttribute('data-theme');
    } else {
      root.setAttribute('data-theme', theme);
    }
  }

  clearError(): void {
    this.error = null;
  }
}

export const settingsStore = new SettingsStore();
