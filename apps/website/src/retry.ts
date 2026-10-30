// Retry timing shared by uploads and downloads. Tests shorten these.
export const timing = { retryBase: 1000, retryMax: 30_000, stall: 60_000, response: 10 * 60_000 };

// Longer waits, such as the rest of a daily limit, end the attempt instead.
export const MAX_RETRY_AFTER = 10 * 60_000;

// Statuses after which the server or a proxy may well answer differently later.
export function retryableStatus(status: number): boolean {
  return (
    status === 408 ||
    status === 425 ||
    status === 429 ||
    (status >= 500 && status !== 501 && status !== 505 && status !== 507)
  );
}

export function parseRetryAfter(value: string | null): number | null {
  if (value === null) return null;
  const seconds = Number(value);
  if (Number.isFinite(seconds)) return Math.max(seconds, 0) * 1000;
  const date = Date.parse(value);
  return Number.isNaN(date) ? null : Math.max(date - Date.now(), 0);
}

export function abortReason(signal: AbortSignal): unknown {
  return signal.reason ?? new DOMException("The transfer was cancelled.", "AbortError");
}

// How long to wait after `failures` failed attempts in a row. The random spread keeps many
// browsers from retrying at the same moment.
export function backoff(failures: number, retryAfter: number | null): number {
  const delay =
    Math.min(timing.retryMax, timing.retryBase * 2 ** failures) * (0.5 + Math.random() / 2);
  return Math.max(delay, retryAfter ?? 0);
}

// Waits before a retry. Coming back online ends the wait early.
export function pause(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    const end = (error?: unknown) => {
      clearTimeout(timer);
      globalThis.removeEventListener?.("online", wake);
      signal?.removeEventListener("abort", stop);
      if (error === undefined) resolve();
      else reject(error);
    };
    const wake = () => end();
    const stop = () => end(abortReason(signal!));
    const timer = setTimeout(wake, ms);
    globalThis.addEventListener?.("online", wake);
    signal?.addEventListener("abort", stop);
  });
}
