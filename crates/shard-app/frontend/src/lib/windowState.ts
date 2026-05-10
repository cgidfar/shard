// Per-window state singleton. Each WebView runs its own JS module
// instance, so this `let` is naturally scoped to one Tauri window.
//
// `init()` resolves the window's Tauri label via the `get_window_label`
// IPC command and caches it. `label` throws if accessed before `init()`
// completes — a misuse signal for callers that race startup.

import { invoke } from "@tauri-apps/api/core";

let cachedLabel: string | null = null;

export const windowState = {
  async init(): Promise<void> {
    if (cachedLabel !== null) return;
    cachedLabel = await invoke<string>("get_window_label");
  },

  get label(): string {
    if (cachedLabel === null) {
      throw new Error("windowState accessed before init()");
    }
    return cachedLabel;
  },

  /** True iff `init()` has completed. Use sparingly — most call sites
   * should `await init()` once at startup and then read `.label`. */
  get ready(): boolean {
    return cachedLabel !== null;
  },
};
