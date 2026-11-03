import { useCallback, useLayoutEffect, useRef } from 'react';

/** Stable event identity with the latest committed state, never speculative render state. */
export function useCommittedCallback<T extends (...args: any[]) => any>(callback: T): T {
  const current = useRef(callback);
  useLayoutEffect(() => { current.current = callback; });
  return useCallback(((...args: Parameters<T>) => current.current(...args)) as T, []);
}
