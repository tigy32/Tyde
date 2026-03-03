import { defineConfig, mergeConfig } from 'vite';
import baseConfig from './vite.config';
import path from 'path';

export default mergeConfig(baseConfig, defineConfig({
  resolve: {
    alias: {
      '@tauri-apps/api/event': path.resolve(__dirname, 'tests/e2e/mocks/tauri-event.ts'),
      '@tauri-apps/api/core': path.resolve(__dirname, 'tests/e2e/mocks/tauri-core.ts'),
      '@tauri-apps/api/window': path.resolve(__dirname, 'tests/e2e/mocks/tauri-window.ts'),
      '@tauri-apps/api/dpi': path.resolve(__dirname, 'tests/e2e/mocks/tauri-dpi.ts'),
      '@tauri-apps/api/webviewWindow': path.resolve(__dirname, 'tests/e2e/mocks/tauri-webview-window.ts'),
      '@tauri-apps/plugin-dialog': path.resolve(__dirname, 'tests/e2e/mocks/tauri-dialog.ts'),
    },
  },
  server: {
    port: 1420,
    strictPort: true,
  },
}));
