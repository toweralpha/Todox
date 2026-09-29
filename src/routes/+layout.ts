// Tauri 没有 Node 服务器，无法做真正的 SSR，
// 因此用 adapter-static 配合 index.html 回退，把应用跑在 SPA 模式。
// 详见 svelte.config.js 与 https://v2.tauri.app/start/frontend/sveltekit/
export const ssr = false;

// 设计系统在这里统一引入，且顺序不可调换：
//   tokens.css 定义变量 → motion.css 引用时长/曲线变量 → global.css 消费两者
// 任何视图都因此自动获得完整的主题能力，无需各自重复引入。
import '$lib/design/tokens.css';
import '$lib/design/motion.css';
import '$lib/design/global.css';

import { settingsStore } from '$lib/stores/settings.svelte';

/**
 * 应用启动时读取设置并应用主题。
 *
 * 放在布局的 load 里而不是各页面的 onMount：主题属于整个应用的显示状态，
 * 若由页面负责，用户切换到某个"忘记应用主题"的视图时界面会闪回默认配色。
 *
 * 读取失败不阻断启动 —— 设置读不出来时用默认主题，应用照常可用。
 */
export async function load() {
  try {
    await settingsStore.load();
    if (settingsStore.settings) {
      settingsStore.applyTheme(settingsStore.settings.theme);
    }
  } catch {
    // 忽略：默认主题已经在 CSS 里定义好了
  }
}
