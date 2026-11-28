import { act, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { Link, MemoryRouter, Route, Routes } from 'react-router-dom';
import { expect, it, vi } from 'vitest';
import MainContent from './MainContent';
import { compactConversation, getConversation, getStreamStatus, getGenerationStatus, getContextSize } from '../api';
import { useChatStore } from '../stores/useChatStore';
import { useProjectStore } from '../stores/useProjectStore';

vi.mock('../api', async importOriginal => {
  const actual = await importOriginal<Record<string, unknown>>();
  return Object.fromEntries(Object.entries(actual).map(([name, value]) => [name, typeof value === 'function' ? vi.fn(() => new Promise(() => {})) : value]));
});

it.each([
  ['switch', 'post'],
  ['remount', 'post'],
  ['remount', 'refresh'],
  ['remount', 'error'],
])('keeps B selected on %s during A’s %s', async (navigation, phase) => {
  Element.prototype.scrollTo = vi.fn();
  let finish!: () => void;
  let fail!: () => void;
  vi.mocked(compactConversation).mockImplementation(() => new Promise((resolve, reject) => {
    fail = () => reject(new Error('Old conversation error'));
    finish = () => resolve({ summary: 'saved A', messagesCompacted: 2, tokensSaved: 3 });
  }));
  vi.mocked(getStreamStatus).mockResolvedValue({ active: false, eventCount: 0 });
  vi.mocked(getGenerationStatus).mockResolvedValue({ active: false } as never);
  vi.mocked(getContextSize).mockImplementation(async id => ({ tokens: id === 'A' ? 10 : 20, limit: 100 }));
  vi.mocked(getConversation).mockImplementation(async id => ({ id, title: id, model: 'm', workspace_mode: 'chat', messages: [
    { id: id + '-user', role: 'user', content: 'Conversation ' + id },
    { id: id + '-reply', role: 'assistant', content: 'Answer ' + id },
  ] } as never));
  const view = (id: string) => render(<MemoryRouter initialEntries={['/chat/' + id]}><Link to="/chat/B">go to B</Link><Routes>
    <Route path="/chat/:id" element={<MainContent onNewChat={vi.fn()} />} />
  </Routes></MemoryRouter>);
  const a = view('A');
  await screen.findByText('Answer A');
  const textarea = document.querySelector('textarea')!;
  fireEvent.change(textarea, { target: { value: '/compact review' } });
  fireEvent.keyDown(textarea, { key: 'Enter' });
  await waitFor(() => expect(compactConversation).toHaveBeenCalledWith('A', 'review'));
  let finishRefresh!: () => void;
  if (phase === 'refresh') {
    let release!: () => void;
    const pending = new Promise<void>(resolve => { release = resolve; });
    const history = vi.mocked(getConversation).getMockImplementation()!;
    vi.mocked(getConversation).mockImplementationOnce(async id => {
      await pending;
      return history(id);
    });
    vi.mocked(getContextSize).mockImplementationOnce(async () => {
      await pending;
      return { tokens: 99, limit: 100 };
    });
    await act(async () => { finish(); });
    finishRefresh = release;
  }
  if (navigation === 'remount') {
    a.unmount();
    view('B');
  } else {
    fireEvent.click(screen.getByText('go to B'));
  }
  await screen.findByText('Answer B');
  await act(async () => {
    if (phase === 'refresh') finishRefresh();
    else if (phase === 'error') fail();
    else finish();
  });
  expect(useChatStore.getState().compactStatus.state).toBe('idle');
  expect(useChatStore.getState().conversationTitle).toBe('B');
  expect(screen.getByText('Answer B')).toBeInTheDocument();
  expect(useProjectStore.getState().contextInfo?.tokens).toBe(20);
});
