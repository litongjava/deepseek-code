import { beforeEach, expect, it } from 'vitest';
import { useRightSidebarStore } from './useRightSidebarStore';
beforeEach(() => useRightSidebarStore.setState({ tab: 'files', tabs: [{ id: 'files', kind: 'files' }], open: false }));
it('deduplicates paths within a scope but distinguishes equal names across roots and scopes', () => {
  const store = useRightSidebarStore.getState();
  const file = { scope: { projectId: 'p1' }, rootId: 'one', relativePath: 'src/file.ts', name: 'file.ts', path: '/one/src/file.ts' };
  store.openFile(file); store.openFile({ ...file, position: { line: 2, startColumn: 3, endColumn: 5 } });
  expect(useRightSidebarStore.getState().tabs).toHaveLength(2);
  store.openFile({ ...file, rootId: 'two' }); store.openFile({ ...file, scope: { projectId: 'p2' } });
  expect(useRightSidebarStore.getState().tabs).toHaveLength(4);
});
it('closes to an adjacent tab and can reopen from the empty state', () => {
  const store = useRightSidebarStore.getState(); store.openTab('browser'); store.openTab('terminal');
  store.closeTab('terminal'); expect(useRightSidebarStore.getState().tab).toBe('browser');
  store.closeTab('browser'); store.closeTab('files'); expect(useRightSidebarStore.getState().tab).toBe('');
  store.openTab('files'); expect(useRightSidebarStore.getState().tab).toBe('files');
});
