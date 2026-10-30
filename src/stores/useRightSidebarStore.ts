import { create } from 'zustand';
import { persist } from 'zustand/middleware';

export type RightSidebarTab = 'files' | 'browser';

export const RIGHT_SIDEBAR_TABS: readonly RightSidebarTab[] = ['files', 'browser'];
export const RIGHT_SIDEBAR_MIN_WIDTH = 280;
export const RIGHT_SIDEBAR_MAX_WIDTH = 900;
export const RIGHT_SIDEBAR_DEFAULT_WIDTH = 360;
const MAX_BROWSER_URL_LENGTH = 4096;

function isStorableBrowserUrl(url: unknown): url is string {
  return typeof url === 'string' && url.length <= MAX_BROWSER_URL_LENGTH && (url === '' || url === 'about:blank' || /^https?:\/\//i.test(url));
}

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
  /** Last page shown in the Browser tab; '' when none. Persisted. */
  browserUrl: string;
  /** Bumped by openBrowser so the Browser tab navigates even to an unchanged URL. Not persisted. */
  browserRequest: number;
  setOpen: (open: boolean) => void;
  toggle: () => void;
  setTab: (tab: RightSidebarTab) => void;
  setWidth: (width: number) => void;
  /** Record the page the Browser tab shows, without requesting navigation. */
  setBrowserUrl: (url: string) => void;
  /** Show the sidebar on the Browser tab and, with a URL, navigate it there. */
  openBrowser: (url?: string) => void;
}

export const useRightSidebarStore = create<RightSidebarState>()(persist((set) => ({
  open: false,
  tab: 'files',
  width: RIGHT_SIDEBAR_DEFAULT_WIDTH,
  browserUrl: '',
  browserRequest: 0,
  setOpen: (open) => set({ open }),
  toggle: () => set(state => ({ open: !state.open })),
  setTab: (tab) => set({ tab: RIGHT_SIDEBAR_TABS.includes(tab) ? tab : 'files' }),
  setWidth: (width) => set({ width: clampRightSidebarWidth(width) }),
  setBrowserUrl: (url) => set({ browserUrl: isStorableBrowserUrl(url) ? url : '' }),
  openBrowser: (url) => set(state => (url && isStorableBrowserUrl(url)
    ? { open: true, tab: 'browser', browserUrl: url, browserRequest: state.browserRequest + 1 }
    : { open: true, tab: 'browser' })),
}), {
  name: 'right-sidebar',
  version: 1,
  partialize: ({ open, tab, width, browserUrl }) => ({ open, tab, width, browserUrl }),
  merge: (persisted, current) => {
    const saved = (persisted && typeof persisted === 'object' ? persisted : {}) as Partial<RightSidebarState>;
    return {
      ...current,
      open: typeof saved.open === 'boolean' ? saved.open : current.open,
      tab: saved.tab && RIGHT_SIDEBAR_TABS.includes(saved.tab) ? saved.tab : current.tab,
      width: typeof saved.width === 'number' ? clampRightSidebarWidth(saved.width) : current.width,
      browserUrl: isStorableBrowserUrl(saved.browserUrl) ? saved.browserUrl : current.browserUrl,
    };
  },
}));
