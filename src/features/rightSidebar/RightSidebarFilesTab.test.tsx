import { act, fireEvent, render, screen, waitFor, within } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import type { FsFileNode } from '../../api';
import { useChatStore } from '../../stores/useChatStore';
import { useProjectStore } from '../../stores/useProjectStore';
import { useUIStore } from '../../stores/useUIStore';
import ProjectWorkspaceBar from '../project/ProjectWorkspaceBar';
import RightSidebarFilesTab from './RightSidebarFilesTab';

const api = vi.hoisted(() => ({
  getConversation: vi.fn(),
  getProject: vi.fn(),
  getProjects: vi.fn(),
  getFileSystemTree: vi.fn(),
  readFileContent: vi.fn(),
}));
vi.mock('../../api', () => api);

const ROOT = '/work/repo';
const file = (name: string, size = 10, dir = ROOT): FsFileNode => ({ name, path: `${dir}/${name}`, is_dir: false, size });
const folder = (name: string, dir = ROOT): FsFileNode => ({ name, path: `${dir}/${name}`, is_dir: true, size: 4096 });

const listings: Record<string, FsFileNode[]> = {
  [ROOT]: [folder('.git'), folder('src'), file('README.md', 42), file('big.log', 5 * 1024 * 1024), file('logo.png', 300)],
  [`${ROOT}/src`]: [file('main.ts', 20, `${ROOT}/src`)],
};

function renderTab(path = '/chat/c1') {
  return render(
    <MemoryRouter initialEntries={[path]}>
      <Routes>
        <Route path="/" element={<RightSidebarFilesTab />} />
        <Route path="/chat/:id" element={<RightSidebarFilesTab />} />
      </Routes>
    </MemoryRouter>,
  );
}

// Shape returned by GET /api/projects/:id (folder trusted, as in the live report).
const PROJECT = {
  id: 'p1', name: 'dsb-test', description: '', instructions: '', workspace_path: ROOT,
  folders: [{ path: ROOT, trusted: true }], is_archived: 0, created_at: '', updated_at: '',
};

/** Home composer: the real project picker wired the way MainContent wires it before a chat exists. */
function renderHomeWithPicker() {
  const ComposerPicker = () => {
    const pendingProjectId = useProjectStore(s => s.pendingProjectId);
    return <ProjectWorkspaceBar projectId={pendingProjectId} onSelect={project => useProjectStore.getState().setPendingProjectId(project.id)} />;
  };
  return render(
    <MemoryRouter initialEntries={['/']}>
      <Routes>
        <Route path="/" element={<><ComposerPicker /><RightSidebarFilesTab /></>} />
      </Routes>
    </MemoryRouter>,
  );
}

const item = (name: string) => screen.getByRole('treeitem', { name });

beforeEach(() => {
  useUIStore.setState({ language: 'en' });
  useProjectStore.setState({ projectList: [], pendingProjectId: null, currentProjectId: null });
  useChatStore.setState({ loading: false });
  api.getConversation.mockResolvedValue({ id: 'c1', workspace_path: ROOT, project_id: null });
  api.getProjects.mockResolvedValue([PROJECT]);
  api.getProject.mockImplementation(async (id: string) => {
    if (id !== PROJECT.id) throw new Error('Request failed: 404');
    return PROJECT;
  });
  api.getFileSystemTree.mockImplementation(async (path: string) => ({ success: true, path, tree: listings[path] ?? [] }));
  api.readFileContent.mockImplementation(async (path: string) => {
    if (path.endsWith('logo.png')) return { success: true, path, content: '', size: 300, truncated: false, binary: true };
    return { success: true, path, content: 'line one\nline two', size: 17, truncated: false, binary: false };
  });
});

afterEach(() => vi.clearAllMocks());

describe('right sidebar Files tab', () => {
  it('loads the root one level deep and expands folders lazily', async () => {
    renderTab();
    expect(await screen.findByRole('tree', { name: 'Workspace files' })).toBeInTheDocument();
    expect(api.getConversation).toHaveBeenCalledWith('c1');
    expect(api.getFileSystemTree).toHaveBeenCalledTimes(1);
    expect(api.getFileSystemTree).toHaveBeenCalledWith(ROOT, 1);
    expect(screen.getByTitle(ROOT)).toHaveTextContent('repo');

    // Hidden folders are listed collapsed and never expanded automatically.
    expect(item('.git')).toHaveAttribute('aria-expanded', 'false');
    expect(item('src')).toHaveAttribute('aria-expanded', 'false');
    expect(screen.queryByRole('treeitem', { name: 'main.ts' })).toBeNull();

    fireEvent.click(item('src'));
    expect(await screen.findByRole('treeitem', { name: 'main.ts' })).toHaveAttribute('aria-level', '2');
    expect(item('src')).toHaveAttribute('aria-expanded', 'true');
    expect(api.getFileSystemTree).toHaveBeenCalledTimes(2);
    expect(api.getFileSystemTree).toHaveBeenLastCalledWith(`${ROOT}/src`, 1);

    // Collapsing and re-expanding reuses the loaded listing.
    fireEvent.click(item('src'));
    expect(screen.queryByRole('treeitem', { name: 'main.ts' })).toBeNull();
    fireEvent.click(item('src'));
    expect(item('main.ts')).toBeInTheDocument();
    expect(api.getFileSystemTree).toHaveBeenCalledTimes(2);
  });

  it('supports arrow-key navigation, expansion and Enter', async () => {
    renderTab();
    await screen.findByRole('tree');
    const git = item('.git');
    expect(git).toHaveAttribute('tabindex', '0');
    act(() => git.focus());
    fireEvent.keyDown(git, { key: 'ArrowDown' });
    expect(item('src')).toHaveFocus();
    fireEvent.keyDown(item('src'), { key: 'ArrowRight' });
    expect(await screen.findByRole('treeitem', { name: 'main.ts' })).toBeInTheDocument();
    fireEvent.keyDown(item('src'), { key: 'ArrowRight' });
    expect(item('main.ts')).toHaveFocus();
    fireEvent.keyDown(item('main.ts'), { key: 'ArrowLeft' });
    expect(item('src')).toHaveFocus();
    fireEvent.keyDown(item('src'), { key: 'ArrowLeft' });
    expect(item('src')).toHaveAttribute('aria-expanded', 'false');
    fireEvent.keyDown(item('src'), { key: 'End' });
    expect(item('logo.png')).toHaveFocus();
    fireEvent.keyDown(item('logo.png'), { key: 'ArrowUp' });
    fireEvent.keyDown(item('big.log'), { key: 'ArrowUp' });
    fireEvent.keyDown(item('README.md'), { key: 'Enter' });
    expect(await screen.findByTestId('file-preview-path')).toHaveTextContent('README.md');
  });

  it('opens a file read-only with its relative path, copies the path and returns to the tree', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
    renderTab();
    await screen.findByRole('tree');
    fireEvent.click(item('src'));
    fireEvent.click(await screen.findByRole('treeitem', { name: 'main.ts' }));

    expect(await screen.findByText(/line two/)).toBeInTheDocument();
    expect(api.readFileContent).toHaveBeenCalledWith(`${ROOT}/src/main.ts`, { maxBytes: 1024 * 1024 });
    expect(screen.getByTestId('file-preview-path')).toHaveTextContent('src/main.ts');
    expect(screen.getByTestId('file-preview-path')).toHaveAttribute('title', `${ROOT}/src/main.ts`);
    expect(screen.queryByRole('tree')).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: 'Copy path' }));
    expect(await screen.findByRole('button', { name: 'Path copied' })).toBeInTheDocument();
    expect(writeText).toHaveBeenCalledWith(`${ROOT}/src/main.ts`);

    fireEvent.click(screen.getByRole('button', { name: 'Back to files' }));
    expect(screen.getByRole('tree')).toBeInTheDocument();
    expect(item('main.ts')).toHaveFocus();
    expect(item('src')).toHaveAttribute('aria-expanded', 'true');
  });

  it('does not fetch files above 1 MB and labels binary files', async () => {
    renderTab();
    await screen.findByRole('tree');
    fireEvent.click(item('big.log'));
    expect(await screen.findByText('File too large to preview')).toBeInTheDocument();
    expect(screen.getByText('5.2 MB')).toBeInTheDocument();
    expect(api.readFileContent).not.toHaveBeenCalled();

    fireEvent.click(screen.getByRole('button', { name: 'Back to files' }));
    fireEvent.click(item('logo.png'));
    expect(await screen.findByText('Binary file')).toBeInTheDocument();
    expect(screen.getByText('300 byte')).toBeInTheDocument();
  });

  it('treats NUL bytes in returned text as binary', async () => {
    api.readFileContent.mockResolvedValue({ success: true, path: `${ROOT}/README.md`, content: 'PK\0\0data' });
    renderTab();
    await screen.findByRole('tree');
    fireEvent.click(item('README.md'));
    expect(await screen.findByText('Binary file')).toBeInTheDocument();
  });

  it('shows the no-workspace state for chats without a folder and on Home without a project', async () => {
    api.getConversation.mockResolvedValue({ id: 'c1', workspace_path: null, project_id: null });
    const { unmount } = renderTab();
    expect(await screen.findByText('This chat has no workspace folder')).toBeInTheDocument();
    expect(api.getFileSystemTree).not.toHaveBeenCalled();
    unmount();

    renderTab('/');
    expect(screen.getByText('This chat has no workspace folder')).toBeInTheDocument();
    expect(api.getConversation).toHaveBeenCalledTimes(1);
  });

  it('follows the project picked in the Home composer even when the store project list is empty', async () => {
    renderHomeWithPicker();
    expect(screen.getByText('This chat has no workspace folder')).toBeInTheDocument();
    expect(useProjectStore.getState().projectList).toEqual([]);

    fireEvent.click(screen.getByRole('button', { name: 'Select project' }));
    fireEvent.click(await screen.findByRole('button', { name: 'dsb-test' }));

    expect(await screen.findByRole('tree', { name: 'Workspace files' })).toBeInTheDocument();
    expect(useProjectStore.getState().pendingProjectId).toBe('p1');
    expect(screen.getByRole('button', { name: 'Select project' })).toHaveTextContent('dsb-test');
    expect(api.getProject).toHaveBeenCalledWith('p1');
    expect(api.getFileSystemTree).toHaveBeenCalledWith(ROOT, 1);
    // The picker also titles its folder chip with the path; the tab header shows the folder name.
    expect(screen.getAllByTitle(ROOT).map(el => el.textContent)).toContain('repo');

    // Clearing the selection (New Chat) returns to the empty state.
    act(() => useProjectStore.getState().setPendingProjectId(null));
    expect(screen.getByText('This chat has no workspace folder')).toBeInTheDocument();
  });

  it('uses a listed project without fetching it again', async () => {
    useProjectStore.setState({ projectList: [{ id: 'p1', name: 'Repo', workspace_path: ROOT }], pendingProjectId: 'p1' });
    renderTab('/');
    expect(await screen.findByRole('tree')).toBeInTheDocument();
    expect(api.getProject).not.toHaveBeenCalled();
  });

  it('shows a project chat from its own workspace_path or, without one, from its project', async () => {
    // Bridge chats created in a project carry the project's folder.
    api.getConversation.mockResolvedValue({ id: 'c1', workspace_path: ROOT, project_id: 'p1' });
    const { unmount } = renderTab();
    expect(await screen.findByRole('tree')).toBeInTheDocument();
    expect(api.getProject).not.toHaveBeenCalled();
    unmount();

    // Only project_id, store list empty: the project is fetched.
    api.getConversation.mockResolvedValue({ id: 'c1', workspace_path: null, project_id: 'p1' });
    renderTab();
    expect(await screen.findByRole('tree')).toBeInTheDocument();
    expect(api.getProject).toHaveBeenCalledWith('p1');
    expect(api.getFileSystemTree).toHaveBeenLastCalledWith(ROOT, 1);
  });

  it('picks up a chat attached to a project while open', async () => {
    api.getConversation.mockResolvedValue({ id: 'c1', workspace_path: null, project_id: null });
    renderTab();
    expect(await screen.findByText('This chat has no workspace folder')).toBeInTheDocument();

    api.getConversation.mockResolvedValue({ id: 'c1', workspace_path: ROOT, project_id: 'p1' });
    act(() => useProjectStore.getState().setCurrentProjectId('p1'));
    expect(await screen.findByRole('tree')).toBeInTheDocument();
    expect(api.getConversation).toHaveBeenCalledTimes(2);
  });

  it('shows folder errors with a retry and refreshes after a chat turn', async () => {
    api.getFileSystemTree.mockRejectedValueOnce(new Error('Failed to get file tree: 404'));
    renderTab();
    expect(await screen.findByText("Couldn't open this folder")).toBeInTheDocument();
    expect(screen.getByText('Failed to get file tree: 404')).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
    expect(await screen.findByRole('tree')).toBeInTheDocument();
    expect(api.getFileSystemTree).toHaveBeenCalledTimes(2);

    listings[ROOT].push(file('new.txt'));
    try {
      act(() => useChatStore.setState({ loading: true }));
      act(() => useChatStore.setState({ loading: false }));
      expect(await screen.findByRole('treeitem', { name: 'new.txt' })).toBeInTheDocument();
      expect(api.getFileSystemTree).toHaveBeenCalledTimes(3);

      fireEvent.click(screen.getByRole('button', { name: 'Refresh files' }));
      await waitFor(() => expect(api.getFileSystemTree).toHaveBeenCalledTimes(4));
      expect(within(screen.getByRole('tree')).getAllByRole('treeitem')).toHaveLength(6);
    } finally {
      listings[ROOT].pop();
    }
  });
});
