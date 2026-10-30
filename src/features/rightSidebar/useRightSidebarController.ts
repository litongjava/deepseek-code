import { useCallback, useEffect, useRef } from 'react';
import { useRightSidebarStore } from '../../stores/useRightSidebarStore';
import { normalizeBrowserUrl } from './browser/browserUrl';

/** Window event that asks for a panel; chat links send `{ panel: 'browser', url }`. */
export const OPEN_PANEL_EVENT = 'request-open-panel';

/** Ctrl+Alt+B (Cmd+Option+B on macOS). Matching `key` leaves AltGr layouts that type a character alone. */
export function isRightSidebarShortcut(event: KeyboardEvent): boolean {
  return (event.ctrlKey || event.metaKey) && event.altKey && !event.shiftKey && event.key.toLowerCase() === 'b';
}

export function useRightSidebarController({ allowed, otherPanelOpen, closeOtherPanels }: {
  /** Whether the current route shows the sidebar (chat views only). */
  allowed: boolean;
  /** A document preview or the artifacts list currently owns the right-panel slot. */
  otherPanelOpen: boolean;
  closeOtherPanels: () => void;
}) {
  const open = useRightSidebarStore(s => s.open);
  const setOpen = useRightSidebarStore(s => s.setOpen);
  const visible = allowed && open && !otherPanelOpen;

  // One right panel at a time. A document preview or the artifacts list takes the
  // slot while it is shown and the sidebar's `open` preference is kept, so closing
  // them brings the sidebar back. Explicitly showing the sidebar dismisses them.
  const toggle = useCallback(() => {
    if (visible) {
      setOpen(false);
      return;
    }
    if (otherPanelOpen) closeOtherPanels();
    setOpen(true);
  }, [visible, otherPanelOpen, closeOtherPanels, setOpen]);

  const toggleRef = useRef(toggle);
  toggleRef.current = toggle;
  useEffect(() => {
    if (!allowed) return;
    const handler = (event: KeyboardEvent) => {
      if (!isRightSidebarShortcut(event)) return;
      event.preventDefault();
      toggleRef.current();
    };
    window.addEventListener('keydown', handler);
    return () => window.removeEventListener('keydown', handler);
  }, [allowed]);

  // Chat links ("Open chat links in: In-App Sidebar") and the left Tools > Browser
  // entry open the Browser tab. The request is honoured on any route; the sidebar
  // shows once a chat view is active.
  const otherPanelRef = useRef({ otherPanelOpen, closeOtherPanels });
  otherPanelRef.current = { otherPanelOpen, closeOtherPanels };
  useEffect(() => {
    const handler = (event: Event) => {
      const detail = (event as CustomEvent<{ panel?: string; url?: unknown } | undefined>).detail;
      if (detail?.panel !== 'browser') return;
      let url: string | undefined;
      if (typeof detail.url === 'string') {
        const result = normalizeBrowserUrl(detail.url);
        if (!result.ok) return;
        url = result.url;
      }
      if (otherPanelRef.current.otherPanelOpen) otherPanelRef.current.closeOtherPanels();
      useRightSidebarStore.getState().openBrowser(url);
    };
    window.addEventListener(OPEN_PANEL_EVENT, handler);
    return () => window.removeEventListener(OPEN_PANEL_EVENT, handler);
  }, []);

  const close = useCallback(() => setOpen(false), [setOpen]);
  return { visible, toggle, close };
}
