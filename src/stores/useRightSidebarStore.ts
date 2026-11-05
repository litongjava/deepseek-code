import { create } from 'zustand';
import { persist } from 'zustand/middleware';
import type { SubagentInvocation } from '../api/subagents';
import type { WorkspaceScope } from '../api/workspaceFiles';

export type RightSidebarTab = 'terminal' | 'files' | 'browser';
export const RIGHT_SIDEBAR_TABS: readonly RightSidebarTab[] = ['terminal', 'files', 'browser'];
export const RIGHT_SIDEBAR_MIN_WIDTH = 280;
export const RIGHT_SIDEBAR_MAX_WIDTH = 900;
export const RIGHT_SIDEBAR_DEFAULT_WIDTH = 560;
const MAX_BROWSER_URL_LENGTH = 4096;

export interface FilePosition { line: number; startColumn: number; endColumn: number }
export interface SidebarFileTab {
  id: string;
  kind: 'file';
  scope: WorkspaceScope;
  rootId: string;
  relativePath: string;
  name: string;
  path: string;
  position?: FilePosition;
  navigation: number;
}
export type SidebarSubagentTab = { id: string; kind: 'subagent'; invocation: SubagentInvocation };
export type AuxiliaryTab = SidebarSubagentTab | { id: RightSidebarTab; kind: RightSidebarTab } | SidebarFileTab;
export const workspaceScopeKey = (scope: WorkspaceScope) => scope.conversationId ? `conversation:${scope.conversationId}` : `project:${scope.projectId ?? ''}`;

function isStorableBrowserUrl(url: unknown): url is string {
  return typeof url === 'string' && url.length <= MAX_BROWSER_URL_LENGTH && (url === '' || url === 'about:blank' || /^https?:\/\//i.test(url));
}

export function clampRightSidebarWidth(width: number, viewportWidth = typeof window === 'undefined' ? 1280 : window.innerWidth): number {
  const max = Math.max(RIGHT_SIDEBAR_MIN_WIDTH, Math.min(RIGHT_SIDEBAR_MAX_WIDTH, Math.floor(viewportWidth * 0.7)));
  return Math.min(max, Math.max(RIGHT_SIDEBAR_MIN_WIDTH, Number.isFinite(width) ? Math.round(width) : RIGHT_SIDEBAR_DEFAULT_WIDTH));
}

export function isRightSidebarRoute(pathname: string): boolean {
  return pathname === '/' || pathname.startsWith('/chat/');
}

interface RightSidebarState {
  open: boolean;
  tab: string;
  tabs: AuxiliaryTab[];
  width: number;
  treeVisible: boolean;
  browserUrl: string;
  browserRequest: number;
  setOpen: (open: boolean) => void;
  toggle: () => void;
  setTab: (tab: string) => void;
  openTab: (tab: RightSidebarTab) => void;
  closeTab: (id: string) => void;
  openSubagent: (invocation: SubagentInvocation) => void;
  openFile: (file: Omit<SidebarFileTab, 'id' | 'kind' | 'navigation'>) => void;
  setWidth: (width: number) => void;
  toggleTree: () => void;
  setBrowserUrl: (url: string) => void;
  openBrowser: (url?: string) => void;
}

export const useRightSidebarStore = create<RightSidebarState>()(persist((set) => ({
  open: false,
  tab: 'files',
  tabs: [{ id: 'files', kind: 'files' }, { id: 'browser', kind: 'browser' }],
  width: RIGHT_SIDEBAR_DEFAULT_WIDTH,
  treeVisible: true,
  browserUrl: '',
  browserRequest: 0,
  setOpen: (open) => set({ open }),
  toggle: () => set(state => ({ open: !state.open })),
  setTab: (tab) => set(state => state.tabs.some(item => item.id === tab) ? { tab } :
    RIGHT_SIDEBAR_TABS.includes(tab as RightSidebarTab) ? { tab, tabs: [...state.tabs, { id: tab as RightSidebarTab, kind: tab as RightSidebarTab }] } : {}),
  openTab: (tab) => set(state => ({ open: true, tab, tabs: state.tabs.some(item => item.id === tab) ? state.tabs : [...state.tabs, { id: tab, kind: tab }] })),
  closeTab: (id) => set(state => {
    const index = state.tabs.findIndex(item => item.id === id);
    if (index < 0) return {};
    const tabs = state.tabs.filter(item => item.id !== id);
    return { tabs, tab: state.tab === id ? (tabs[Math.min(index, tabs.length - 1)]?.id ?? '') : state.tab };
  }),
  openSubagent: (invocation) => set(state => {
    const id = `subagent:${invocation.child_session_id}`;
    const value: SidebarSubagentTab = { id, kind: 'subagent', invocation };
    return { open: true, tab: id, tabs: state.tabs.some(item => item.id === id) ? state.tabs.map(item => item.id === id ? value : item) : [...state.tabs, value] };
  }),
  openFile: (file) => set(state => {
    const relativePath = file.relativePath.replace(/\\/g, '/');
    const id = `file:${JSON.stringify([workspaceScopeKey(file.scope), file.rootId, relativePath])}`;
    const existing = state.tabs.find(item => item.id === id);
    const value: SidebarFileTab = { ...file, relativePath, id, kind: 'file', navigation: existing?.kind === 'file' ? existing.navigation + 1 : 1 };
    return { open: true, tab: id, tabs: existing ? state.tabs.map(item => item.id === id ? value : item) : [...state.tabs, value] };
  }),
  setWidth: (width) => set({ width: clampRightSidebarWidth(width) }),
  toggleTree: () => set(state => ({ treeVisible: !state.treeVisible })),
  setBrowserUrl: (url) => set({ browserUrl: isStorableBrowserUrl(url) ? url : '' }),
  openBrowser: (url) => set(state => ({
    open: true, tab: 'browser',
    tabs: state.tabs.some(item => item.id === 'browser') ? state.tabs : [...state.tabs, { id: 'browser', kind: 'browser' }],
    ...(url && isStorableBrowserUrl(url) ? { browserUrl: url, browserRequest: state.browserRequest + 1 } : {}),
  })),
}), {
  name: 'right-sidebar',
  version: 1,
  // Processes and file capabilities are deliberately not restored from localStorage.
  partialize: ({ open, tab, width, browserUrl, treeVisible }) => ({ open, tab: RIGHT_SIDEBAR_TABS.includes(tab as RightSidebarTab) ? tab : 'files', width, browserUrl, treeVisible }),
  merge: (persisted, current) => {
    const saved = (persisted && typeof persisted === 'object' ? persisted : {}) as Partial<RightSidebarState>;
    const tab = saved.tab && RIGHT_SIDEBAR_TABS.includes(saved.tab as RightSidebarTab) ? saved.tab as RightSidebarTab : 'files';
    return {
      ...current,
      open: typeof saved.open === 'boolean' ? saved.open : current.open,
      tab: tab === 'terminal' ? 'files' : tab,
      width: typeof saved.width === 'number' ? clampRightSidebarWidth(saved.width) : current.width,
      treeVisible: typeof saved.treeVisible === 'boolean' ? saved.treeVisible : true,
      browserUrl: isStorableBrowserUrl(saved.browserUrl) ? saved.browserUrl : current.browserUrl,
    };
  },
}));
