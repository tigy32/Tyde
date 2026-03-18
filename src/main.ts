import "highlight.js/styles/github-dark.css";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { AppController } from "./app";

// Kill orphaned subprocesses from a previous webview session (e.g. page refresh).
// HMR patches modules in-place without re-executing main.ts, so this only fires on full reloads.
invoke("shutdown_all_subprocesses").catch((e: unknown) => {
  console.warn("Failed to shutdown orphaned subprocesses:", e);
});

const WINDOW_STATE_KEY = "tyde-window-state";

interface WindowState {
  x: number;
  y: number;
  width: number;
  height: number;
  maximized: boolean;
}

async function saveWindowState(): Promise<void> {
  try {
    const win = getCurrentWindow();
    const pos = await win.outerPosition();
    const size = await win.outerSize();
    const maximized = await win.isMaximized();
    const state: WindowState = {
      x: pos.x,
      y: pos.y,
      width: size.width,
      height: size.height,
      maximized,
    };
    localStorage.setItem(WINDOW_STATE_KEY, JSON.stringify(state));
  } catch (err) {
    console.error("Failed to save window state:", err);
  }
}

async function restoreWindowState(): Promise<void> {
  const raw = localStorage.getItem(WINDOW_STATE_KEY);
  if (!raw) return;

  try {
    const state: WindowState = JSON.parse(raw);
    const win = getCurrentWindow();

    if (state.maximized) {
      await win.maximize();
      return;
    }

    const { PhysicalPosition, PhysicalSize } = await import(
      "@tauri-apps/api/dpi"
    );
    await win.setPosition(new PhysicalPosition(state.x, state.y));
    await win.setSize(new PhysicalSize(state.width, state.height));
  } catch (err) {
    console.error("Failed to restore window state:", err);
    localStorage.removeItem(WINDOW_STATE_KEY);
  }
}

// Prevent the browser from navigating to files dropped outside of handled drop zones.
document.addEventListener("dragover", (e) => e.preventDefault());
document.addEventListener("drop", (e) => e.preventDefault());

document.addEventListener("DOMContentLoaded", async () => {
  await restoreWindowState();
  const app = new AppController();

  window.onerror = (message, source, lineno, colno, error) => {
    console.error("Unhandled error:", {
      message,
      source,
      lineno,
      colno,
      error,
    });
    const msg = error?.message || String(message);
    app.showError(`Unexpected error: ${msg.slice(0, 200)}`);
    return false;
  };

  window.addEventListener("unhandledrejection", (event) => {
    console.error("Unhandled promise rejection:", event.reason);
    const msg = event.reason?.message || String(event.reason);
    app.showError(`Unexpected error: ${msg.slice(0, 200)}`);
  });

  await app.init();

  window.addEventListener("beforeunload", () => {
    app.persistActiveProjectUiState();
    saveWindowState();
  });
});
