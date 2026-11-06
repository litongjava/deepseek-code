import { useEffect, useState } from 'react';

/** Phones and narrow windows: the sidebar becomes an overlay drawer. */
export const NARROW_SCREEN_QUERY = '(max-width: 767px)';

export function isNarrowScreen(): boolean {
  return typeof window !== 'undefined' && typeof window.matchMedia === 'function'
    && window.matchMedia(NARROW_SCREEN_QUERY).matches;
}

export function useNarrowScreen(): boolean {
  const [narrow, setNarrow] = useState(isNarrowScreen);
  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return;
    const query = window.matchMedia(NARROW_SCREEN_QUERY);
    const update = () => setNarrow(query.matches);
    update();
    query.addEventListener?.('change', update);
    return () => query.removeEventListener?.('change', update);
  }, []);
  return narrow;
}
