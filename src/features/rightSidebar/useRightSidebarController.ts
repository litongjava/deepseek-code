import { useCallback, useEffect, useRef } from 'react';
import { useRightSidebarStore } from '../../stores/useRightSidebarStore';

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

  const close = useCallback(() => setOpen(false), [setOpen]);
  return { visible, toggle, close };
}
