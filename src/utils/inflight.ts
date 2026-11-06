const pending = new Map<string, Promise<unknown>>();

/**
 * Shares one in-flight read among concurrent callers with the same key, so
 * components that mount together send a single request. Settled results are
 * not cached: the next call after completion reads fresh data.
 */
export function shareInFlight<T>(key: string, read: () => Promise<T>): Promise<T> {
  const existing = pending.get(key) as Promise<T> | undefined;
  if (existing) return existing;
  const promise = read().finally(() => pending.delete(key));
  pending.set(key, promise);
  return promise;
}
