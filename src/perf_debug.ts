const TAB_PERF_DEBUG_KEY = "tyde-debug-tab-switch";
const TAB_PERF_SLOW_MS = 120;

type TabPerfDetails = Record<string, unknown>;

function roundIfNumber(value: unknown): unknown {
  if (typeof value !== "number" || !Number.isFinite(value)) return value;
  return Number(value.toFixed(1));
}

export function perfNow(): number {
  if (
    typeof performance !== "undefined" &&
    typeof performance.now === "function"
  ) {
    return performance.now();
  }
  return Date.now();
}

export function isTabPerfDebugEnabled(): boolean {
  const win = window as Window & { __TYDE_DEBUG_TAB_SWITCH?: boolean };
  if (typeof win.__TYDE_DEBUG_TAB_SWITCH === "boolean")
    return win.__TYDE_DEBUG_TAB_SWITCH;

  const stored = localStorage.getItem(TAB_PERF_DEBUG_KEY);
  return stored === "1" || stored === "true" || stored === "on";
}

export function logTabPerf(
  label: string,
  durationMs: number,
  details: TabPerfDetails = {},
): void {
  const debugEnabled = isTabPerfDebugEnabled();
  const slow = durationMs >= TAB_PERF_SLOW_MS;
  if (!debugEnabled && !slow) return;

  const payload: TabPerfDetails = {
    durationMs: roundIfNumber(durationMs),
  };
  for (const [key, value] of Object.entries(details)) {
    payload[key] = roundIfNumber(value);
  }

  if (slow) {
    console.warn(`[tab-perf][slow] ${label}`, payload);
  } else {
    console.debug(`[tab-perf] ${label}`, payload);
  }
}
