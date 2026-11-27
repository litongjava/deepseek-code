import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { FileMemoryPanel } from './FileMemoryPanel';
import { fileMemoryApi } from '../api/fileMemory';
import { useChatStore } from '../stores/useChatStore';
import { useProjectStore } from '../stores/useProjectStore';
vi.mock('../api/fileMemory', () => ({ fileMemoryApi: { list: vi.fn(), save: vi.fn(), remove: vi.fn() } }));
vi.mock('./MemoryPanel', () => ({ MemoryPanel: () => <div>Legacy archive panel</div> }));
const item = { id: 'tests.md', name: 'Testing', description: 'Testing preferences', type: 'user' as const, content: 'original Markdown', version: 'hash-1' };
beforeEach(() => {
  vi.clearAllMocks();
  vi.spyOn(window, 'confirm').mockReturnValue(true);
  useChatStore.setState({ conversationId: 'session-1' });
  useProjectStore.setState({ currentProjectId: 'project-1' });
  vi.mocked(fileMemoryApi.list).mockResolvedValue({ scope: 'scope', directory: '/memory', generation: 1, items: [item], index: '', version: 'index-1', omitted: 0, diagnostics: [] });
  vi.mocked(fileMemoryApi.save).mockResolvedValue({});
  vi.mocked(fileMemoryApi.remove).mockResolvedValue({});
});
it('uses the persisted session scope and forwards CAS when editing and deleting', async () => {
  render(<FileMemoryPanel />);
  fireEvent.click(await screen.findByText('Edit Testing'));
  expect(fileMemoryApi.list).toHaveBeenCalledWith({ session_id: 'session-1' }, '');
  fireEvent.change(screen.getByLabelText('Markdown'), { target: { value: 'updated Markdown' } });
  fireEvent.click(screen.getByText('Save memory'));
  await waitFor(() => expect(fileMemoryApi.save).toHaveBeenCalledWith({ session_id: 'session-1' }, 'tests.md', 'updated Markdown', 'hash-1'));
  await waitFor(() => expect(screen.getByText('Delete Testing')).not.toBeDisabled());
  fireEvent.click(screen.getByText('Delete Testing'));
  await waitFor(() => expect(fileMemoryApi.remove).toHaveBeenCalledWith({ session_id: 'session-1' }, item));
});
it('preserves the draft on conflict and offers refresh', async () => {
  vi.mocked(fileMemoryApi.save).mockRejectedValue(new Error('Conflict'));
  render(<FileMemoryPanel />);
  fireEvent.click(await screen.findByText('Edit Testing'));
  fireEvent.click(screen.getByText('Save memory'));
  expect(await screen.findByRole('alert')).toHaveTextContent('Conflict');
  expect(screen.getByLabelText('Markdown')).toHaveValue('original Markdown');
});
it('requires a selected scope and keeps legacy access explicit', async () => {
  useChatStore.setState({ conversationId: null });useProjectStore.setState({ currentProjectId: null });
  render(<FileMemoryPanel />);
  expect(screen.getByText(/Select a trusted project/)).toBeInTheDocument();
  expect(fileMemoryApi.list).not.toHaveBeenCalled();
  fireEvent.click(screen.getByText('Legacy archive / remote settings'));
  expect(screen.getByText('Legacy archive panel')).toBeInTheDocument();
});

it('keeps the memory when deletion is cancelled', async () => {
  vi.spyOn(window, 'confirm').mockReturnValue(false);
  render(<FileMemoryPanel />);
  fireEvent.click(await screen.findByText('Delete Testing'));
  expect(fileMemoryApi.remove).not.toHaveBeenCalled();
});
