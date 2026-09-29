import { defineConfig } from "vite";
import { sveltekit } from "@sveltejs/kit/vite";
// @ts-expect-error type error without @types/node package
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

// https://vite.dev/config/
export default defineConfig(() => ({
  plugins: [sveltekit()],

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || "127.0.0.1",
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      //
      // 以下三类必须忽略，否则会出真实故障而不只是噪音：
      //   - src-tauri：Rust 编译产物频繁变动，且与前端无关
      //   - build：前端构建输出目录。开发时若执行过 pnpm build，
      //     产物落在监听范围内会触发成百次无意义的页面重载
      //   - 编辑器写文件时产生的临时目录（形如 .README.md.12345.xxxx.tmpdir）：
      //     这些文件存在时间极短且常处于被占用状态，文件监听器试图监视它们时会
      //     抛出未捕获的 EBUSY 异常，直接导致整个开发服务器进程退出。
      ignored: [
        "**/src-tauri/**",
        "**/build/**",
        "**/.svelte-kit/**",
        "**/.*.tmpdir/**",
        "**/*.tmp",
      ],
    },
  },
}));
