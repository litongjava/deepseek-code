import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type RightSidebarTab = 'files' | 'browser';

export const RIGHT_SIDEBAR_TABS: readonly RightSidebarTab[] = ['files', 'browser'];
export const RIGHT_SIDEBAR_MIN_WIDTH = 280;
export const RIGHT_SIDEBAR_MAX_WIDTH = 900;
export const RIGHT_SIDEBAR_DEFAULT_WIDTH = 360;

/** Clamp a sidebar width to 280px..min(900px, 70% of the viewport). */
export function clampRightSidebarWidth(width: number, viewportWidth = typeof window === 'undefined' ? 1280 : window.innerWidth): number {
  const max = Math.max(RIGHT_SIDEBAR_MIN_WIDTH, Math.min(RIGHT_SIDEBAR_MAX_WIDTH, Math.floor(viewportWidth * 0.7)));
  const value = Number.isFinite(width) ? Math.round(width) : RIGHT_SIDEBAR_DEFAULT_WIDTH;
  return Math.min(max, Math.max(RIGHT_SIDEBAR_MIN_WIDTH, value));
}

/** The sidebar belongs to the chat views only (Home and /chat/:id). */
export function isRightSidebarRoute(pathname: string): boolean {
  return pathname === '/' || pathname.startsWith('/chat/');
}

interface RightSidebarState {
  open: boolean;
  tab: RightSidebarTab;
  width: number;
  setOpen: (open: boolean) => void;
  toggle: () => void;
  setTab: (tab: RightSidebarTab) => void;
  setWidth: (width: number) => void;
}

export const useRightSidebarStore = create<RightSidebarState>()(persist((set) => ({
  open: false,
  tab: 'files',
  width: RIGHT_SIDEBAR_DEFAULT_WIDTH,
  setOpen: (open) => set({ open }),
  toggle: () => set(state => ({ open: !state.open })),
  setTab: (tab) => set({ tab: RIGHT_SIDEBAR_TABS.includes(tab) ? tab : 'files' }),
  setWidth: (width) => set({ width: clampRightSidebarWidth(width) }),
}), {
  name: 'right-sidebar',
  version: 1,
  partialize: ({ open, tab, width }) => ({ open, tab, width }),
  merge: (persisted, current) => {
    const saved = (persisted && typeof persisted === 'object' ? persisted : {}) as Partial<RightSidebarState>;
    return {
      ...current,
      open: typeof saved.open === 'boolean' ? saved.open : current.open,
      tab: saved.tab && RIGHT_SIDEBAR_TABS.includes(saved.tab) ? saved.tab : current.tab,
      width: typeof saved.width === 'number' ? clampRightSidebarWidth(saved.width) : current.width,
    };
  },
}));
