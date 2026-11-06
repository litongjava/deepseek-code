import { useEffect, useRef } from 'react';

/**
 * Calls `poll` (now, unless `immediate` is false) and then `intervalMs` after
 * each call settles, so slow requests never overlap. Polling pauses while the
 * page is hidden and runs once as soon as it is visible again. Returns a stop
 * function: a drop-in replacement for setInterval/clearInterval polling.
 */
export function startVisiblePolling(poll: () => unknown, intervalMs: number, { immediate = true } = {}): () => void {
  let stopped = false;
  let running = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const schedule = () => {
    clearTimeout(timer);
    if (!stopped && !document.hidden) timer = setTimeout(run, intervalMs);
  };
  const run = async () => {
    clearTimeout(timer);
    if (stopped || running) return;
    running = true;
    try { await poll(); } catch { /* The caller reports its own errors. */ }
    running = false;
    schedule();
  };
  const onVisible = () => { if (!document.hidden) void run(); };
  document.addEventListener('visibilitychange', onVisible);
  if (immediate) void run(); else schedule();
  return () => { stopped = true; clearTimeout(timer); document.removeEventListener('visibilitychange', onVisible); };
}

/** Hook form of `startVisiblePolling`; restarts when `deps` change. */
export function useVisiblePolling(poll: () => unknown, intervalMs: number, deps: React.DependencyList): void {
  const latest = useRef(poll);
  latest.current = poll;
  // eslint-disable-next-line react-hooks/exhaustive-deps
  useEffect(() => startVisiblePolling(() => latest.current(), intervalMs), [intervalMs, ...deps]);
}
