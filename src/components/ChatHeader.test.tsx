import { beforeEach, describe, expect, it, vi } from 'vitest';
import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes, useNavigate } from 'react-router-dom';
import ChatHeader from './ChatHeader';
import { useChatStore } from '../stores/useChatStore';
import { useConversationViewStore } from '../stores/useConversationViewStore';
import { useGeneralSettingsStore } from '../stores/useGeneralSettingsStore';
import { copyToClipboard } from '../utils/clipboard';

vi.mock('../api', () => ({ getConversation: vi.fn().mockResolvedValue({}), updateConversation: vi.fn(), deleteConversation: vi.fn(), exportConversation: vi.fn() }));
vi.mock('../utils/clipboard', () => ({ copyToClipboard: vi.fn() }));
vi.mock('../utils/tauriAPI', () => ({ tauriAPI: { isTauri: false } }));
let navigate: ReturnType<typeof useNavigate>;
function Header() {
  navigate = useNavigate();
  return <ChatHeader title="Title" showArtifacts={false} documentPanelDoc={null} onOpenArtifacts={vi.fn()} hasArtifacts />;
}
const setup = () => render(<MemoryRouter initialEntries={['/chat/full-session-one']}><Routes><Route path="/chat/:id" element={<Header />} /></Routes></MemoryRouter>);
const copy = () => {
  fireEvent.click(screen.getByRole('button', { name: 'Session options' }));
  fireEvent.click(screen.getByRole('button', { name: 'Copy session ID' }));
};
beforeEach(() => {
  vi.clearAllMocks();
  useChatStore.setState({ conversationTitle: 'A very long session title '.repeat(20) });
  useGeneralSettingsStore.setState({ showCodingView: true });
  useConversationViewStore.setState({ view: 'chat' });
});
describe('session header', () => {
  it('offers reading mode next to Chat even when Coding View is off', async () => {
    useGeneralSettingsStore.setState({ showCodingView: false });
    setup();
    expect(screen.getAllByRole('tab').map(tab => tab.textContent)).toEqual(['Chat', 'Reading mode']);
    fireEvent.click(screen.getByRole('tab', { name: 'Reading mode' }));
    expect(useConversationViewStore.getState().view).toBe('reading');
    expect(screen.getByRole('tab', { name: 'Reading mode' })).toHaveAttribute('aria-selected', 'true');
    fireEvent.click(screen.getByRole('tab', { name: 'Chat' }));
    expect(useConversationViewStore.getState().view).toBe('chat');
    await act(async () => {});
  });
  it('copies the actual full route ID after navigation and reports success only on success', async () => {
    vi.mocked(copyToClipboard).mockResolvedValue(true);
    setup();
    copy();
    expect(await screen.findByRole('status')).toHaveTextContent('Session ID copied');
    expect(copyToClipboard).toHaveBeenLastCalledWith('full-session-one');
    act(() => navigate('/chat/full-session-two'));
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
    copy();
    await waitFor(() => expect(copyToClipboard).toHaveBeenLastCalledWith('full-session-two'));
  });
  it.each([false, 'throw'])('reports clipboard failure (%s)', async result => {
    if (result === 'throw') vi.mocked(copyToClipboard).mockRejectedValue(new Error('denied'));
    else vi.mocked(copyToClipboard).mockResolvedValue(false);
    setup();
    copy();
    expect(await screen.findByRole('alert')).toHaveTextContent('Could not copy session ID');
    expect(screen.queryByRole('status')).not.toBeInTheDocument();
  });
  it('keeps title and view tabs on one row and preserves the remaining actions', async () => {
    const { container } = setup();
    const header = container.querySelector('[data-session-header]')!;
    expect(header).toContainElement(screen.getByRole('tablist'));
    expect(screen.getByTitle(useChatStore.getState().conversationTitle.trim())).toHaveClass('truncate', 'min-w-0');
    fireEvent.click(screen.getByRole('tab', { name: 'Trajectory' }));
    expect(useConversationViewStore.getState().view).toBe('trajectory');
    expect(screen.getByRole('tab', { name: 'Trajectory' })).toHaveAttribute('aria-selected', 'true');
    fireEvent.click(screen.getByRole('tab', { name: 'Chat' }));
    expect(useConversationViewStore.getState().view).toBe('chat');
    expect(screen.queryByRole('button', { name: 'H5' })).not.toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '更多操作' })).not.toBeInTheDocument();
    const listener = vi.fn();
    window.addEventListener('request-open-panel', listener);
    for (const label of ['Terminal', 'Files', 'Browser']) {
      fireEvent.click(screen.getByRole('button', { name: label }));
      expect(listener.mock.calls[listener.mock.calls.length - 1][0].detail).toEqual({ panel: label.toLowerCase() });
    }
    window.removeEventListener('request-open-panel', listener);
    const actions = screen.getByTitle('View Artifacts').parentElement!;
    expect([...actions.querySelectorAll('button')].map(button => button.title)).toEqual(['Terminal', 'Files', 'Browser', 'View Artifacts']);
    fireEvent.click(screen.getByRole('button', { name: 'Session options' }));
    expect(screen.getByRole('button', { name: 'Rename' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Delete' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: '导出会话' })).toBeInTheDocument();
    await act(async () => {});
  });
});
