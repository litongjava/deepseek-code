import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useChatStore } from '../../stores/useChatStore';
import { useProjectStore } from '../../stores/useProjectStore';
import { useUIStore } from '../../stores/useUIStore';
import { useRightSidebarStore } from '../../stores/useRightSidebarStore';
import RightSidebarFilesTab from './RightSidebarFilesTab';

const api = vi.hoisted(() => ({ getWorkspaceRoots: vi.fn(), listWorkspaceTree: vi.fn(), readWorkspaceFile: vi.fn(), searchWorkspaceFiles: vi.fn(), getWorkspaceFileDiff: vi.fn(), getWorkspaceFileCapabilities: vi.fn(), openWorkspaceFile: vi.fn(), saveWorkspaceFileAs: vi.fn() }));
vi.mock('../../api/workspaceFiles', () => api);
const root = { rootId: 'root-1', name: 'repo', path: '/work/repo' };
const entry = (relativePath: string, kind = 'file') => ({ rootId: root.rootId, relativePath, name: relativePath.split('/').pop(), kind, size: 10 });
function renderTab(path = '/chat/c1') {
  return render(<MemoryRouter initialEntries={[path]}><Routes><Route path="/" element={<RightSidebarFilesTab />} /><Route path="/chat/:id" element={<RightSidebarFilesTab />} /></Routes></MemoryRouter>);
}
const item = (name: string) => screen.getByRole('treeitem', { name });
beforeEach(() => {
  vi.resetAllMocks();
  useUIStore.setState({ language: 'en' });
  useProjectStore.setState({ projectList: [], pendingProjectId: null, currentProjectId: null });
  useChatStore.setState({ loading: false });
  useRightSidebarStore.setState({ tab: 'files', tabs: [{ id: 'files', kind: 'files' }], treeVisible: true });
  api.getWorkspaceRoots.mockResolvedValue({ roots: [root] });
  api.listWorkspaceTree.mockImplementation(async (_scope, _root, path) => ({ entries: path === '' ? [entry('src', 'directory'), entry('readme.txt'), entry('logo.png')] : [entry('src/main.ts')], truncated: false }));
  api.readWorkspaceFile.mockImplementation(async (_scope, file) => ({ ...file, path: `${root.path}/${file.relativePath}`, content: 'line one\nline two', size: 17, truncated: false, binary: false, notice: null }));
  api.getWorkspaceFileCapabilities.mockResolvedValue({ reveal: true, defaultApp: true, terminal: true, saveAs: true, wslDistributions: [], reason: 'WSL is not installed.' });
  api.getWorkspaceFileDiff.mockResolvedValue({ patch: '', notice: null, baselineOid: 'abcdef123456', baseline: 'head', status: 'unchanged' });
  api.searchWorkspaceFiles.mockResolvedValue({ matches: [{ rootId: root.rootId, relativePath: 'src/main.ts', line: 2, startColumn: 6, endColumn: 9, preview: 'line two' }], scannedFiles: 2, skippedFiles: 0, truncated: false, notice: null });
});
afterEach(() => vi.clearAllMocks());

describe('scoped Files workspace', () => {
  it('uses server-resolved scope and lazy keyboard-accessible directories', async () => {
    renderTab(); await screen.findByRole('tree');
    expect(api.getWorkspaceRoots).toHaveBeenCalledWith({ conversationId: 'c1' }, expect.any(AbortSignal));
    expect(api.listWorkspaceTree).toHaveBeenCalledWith({ conversationId: 'c1' }, root.rootId, '');
    act(() => item('src').focus()); fireEvent.keyDown(item('src'), { key: 'ArrowRight' });
    expect(await screen.findByRole('treeitem', { name: 'main.ts' })).toHaveAttribute('aria-level', '2');
    fireEvent.keyDown(item('src'), { key: 'ArrowRight' }); expect(item('main.ts')).toHaveFocus();
    fireEvent.keyDown(item('main.ts'), { key: 'Enter' });
    await screen.findByRole('region', { name: 'src/main.ts' });
    expect(item('src')).toHaveAttribute('aria-expanded', 'true');
  });
  it('opens deduplicated file tabs while preserving tree expansion and scroll', async () => {
    renderTab(); await screen.findByRole('tree'); fireEvent.click(item('src'));
    const main = await screen.findByRole('treeitem', { name: 'main.ts' });
    const tree = screen.getByTestId('workspace-file-tree'); tree.scrollTop = 48;
    fireEvent.click(main); await screen.findByText(/line two/);
    expect(screen.getByRole('tree')).toBeVisible(); expect(tree.scrollTop).toBe(48);
    fireEvent.click(item('readme.txt')); await screen.findByRole('region', { name: 'readme.txt' });
    fireEvent.click(main);
    expect(useRightSidebarStore.getState().tabs.filter(tab => tab.kind === 'file')).toHaveLength(2);
    expect(item('src')).toHaveAttribute('aria-expanded', 'true');
    fireEvent.click(screen.getByRole('button', { name: 'Hide file tree' })); expect(tree).not.toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: 'Show file tree' })); expect(tree).toBeVisible(); expect(tree.scrollTop).toBe(48);
  });
  it('shows binary, bounded and read-error states from the scoped backend', async () => {
    api.readWorkspaceFile.mockResolvedValueOnce({ content: null, binary: true, size: 300, notice: null });
    renderTab(); await screen.findByRole('tree'); fireEvent.click(item('logo.png'));
    expect(await screen.findByText('Binary file · 300 bytes')).toBeVisible();
    api.readWorkspaceFile.mockRejectedValueOnce(new Error('Access denied'));
    fireEvent.click(item('readme.txt')); expect(await screen.findByRole('alert')).toHaveTextContent('Access denied');
    api.readWorkspaceFile.mockResolvedValueOnce({ content: null, binary: false, size: 2000000, truncated: true, notice: 'File too large to preview' });
    fireEvent.click(screen.getByRole('button', { name: 'Retry' })); expect(await screen.findByText('File too large to preview')).toBeVisible();
  });
  it('uses selected Home project and shows an empty state without one', async () => {
    renderTab('/'); expect(await screen.findByText('This chat has no workspace folder')).toBeVisible();
    expect(api.getWorkspaceRoots).not.toHaveBeenCalled();
    act(() => useProjectStore.getState().setPendingProjectId('p1'));
    await screen.findByRole('tree'); expect(api.getWorkspaceRoots).toHaveBeenCalledWith({ projectId: 'p1' }, expect.any(AbortSignal));
  });
  it('retries scope failures and refreshes after a turn', async () => {
    api.getWorkspaceRoots.mockRejectedValueOnce(new Error('Untrusted project'));
    renderTab(); expect(await screen.findByRole('alert')).toHaveTextContent('Untrusted project');
    fireEvent.click(screen.getByRole('button', { name: 'Retry' })); await screen.findByRole('tree');
    act(() => useChatStore.setState({ loading: true })); act(() => useChatStore.setState({ loading: false }));
    await waitFor(() => expect(api.listWorkspaceTree).toHaveBeenCalledTimes(2));
  });
  it('searches the project and opens the exact UTF-16 matching position', async () => {
    renderTab(); await screen.findByRole('tree'); fireEvent.click(screen.getByRole('button', { name: 'Search In Files' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Search In Files' }), { target: { value: 'two' } });
    fireEvent.click(await screen.findByRole('button', { name: /src\/main.ts:2:6/ }));
    await waitFor(() => expect(document.querySelector('[data-search-match]')).toHaveTextContent('two'));
    expect(api.searchWorkspaceFiles).toHaveBeenCalledWith({ conversationId: 'c1' }, 'two', expect.any(AbortSignal));
    expect(screen.getByRole('tree')).toBeVisible();
  });
  it('labels real HEAD diff, unchanged and non-Git responses', async () => {
    renderTab(); await screen.findByRole('tree'); fireEvent.click(item('readme.txt')); await screen.findByRole('region', { name: 'readme.txt' });
    fireEvent.click(screen.getByRole('button', { name: 'View file diff' }));
    expect(await screen.findByText('Working tree vs HEAD · abcdef12')).toBeVisible(); expect(screen.getByText('No differences.')).toBeVisible();
    fireEvent.click(screen.getByRole('button', { name: 'View file diff' }));
    api.getWorkspaceFileDiff.mockResolvedValueOnce({ patch: '', notice: 'Not a Git repository', baseline: 'none', status: 'not-git' });
    fireEvent.click(screen.getByRole('button', { name: 'View file diff' })); expect(await screen.findByText('No Git baseline')).toBeVisible();
  });
  it('connects native actions, disables WSL and reports save cancellation/errors honestly', async () => {
    renderTab(); await screen.findByRole('tree'); fireEvent.click(item('readme.txt')); await screen.findByRole('region', { name: 'readme.txt' });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Open' })).toBeEnabled());
    fireEvent.click(screen.getByRole('button', { name: 'Open' }));
    expect(api.openWorkspaceFile).toHaveBeenCalledWith({ conversationId: 'c1' }, expect.objectContaining({ relativePath: 'readme.txt' }), 'reveal', undefined);
    await waitFor(() => expect(screen.getByRole('button', { name: 'Choose opening method' })).toBeEnabled());
    fireEvent.click(screen.getByRole('button', { name: 'Choose opening method' })); expect(screen.getByRole('menuitem', { name: /WSL/ })).toBeDisabled();
    api.saveWorkspaceFileAs.mockResolvedValueOnce({ status: 'cancelled' }); fireEvent.click(screen.getByRole('menuitem', { name: 'Save as…' }));
    await waitFor(() => expect(api.saveWorkspaceFileAs).toHaveBeenCalled()); expect(screen.queryByText('File saved.')).toBeNull();
    api.openWorkspaceFile.mockRejectedValueOnce(new Error('No default application'));
    await waitFor(() => expect(screen.getByRole('button', { name: 'Choose opening method' })).toBeEnabled());
    fireEvent.click(screen.getByRole('button', { name: 'Choose opening method' })); fireEvent.click(screen.getByRole('menuitem', { name: 'Default app' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('No default application');
  });
});
