import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { compactConversation, getContextSize } from '../../api';
import { useChatStore } from '../../stores/useChatStore';
import { useProjectStore } from '../../stores/useProjectStore';
import { handleSlashCommand } from '../slash-commands/registry';
import CompactDialog from './CompactDialog';
import { runManualCompact } from './runManualCompact';

vi.mock('../../api', () => ({ compactConversation: vi.fn(), getContextSize: vi.fn() }));

const reload = vi.fn().mockResolvedValue(undefined);
const compact = (id: string, instructions: string) => runManualCompact(id, instructions, reload, () => true);

beforeEach(() => {
  vi.clearAllMocks();
  useChatStore.getState().setCompactStatus({ state: 'idle' });
});

it('submits dialog focus and refreshes the conversation, count and context estimate', async () => {
  vi.mocked(compactConversation).mockResolvedValue({ summary: 'Keep this', tokensSaved: 125, messagesCompacted: 3 });
  vi.mocked(getContextSize).mockResolvedValue({ tokens: 75, limit: 1000 });
  render(<CompactDialog activeId="session" compactInstruction="  preserve tests  " onCompact={compact}
    setCompactInstruction={vi.fn()} setShowCompactDialog={vi.fn()} />);
  fireEvent.click(screen.getByRole('button', { name: 'Compact' }));
  await waitFor(() => expect(reload).toHaveBeenCalledWith('session', expect.any(Function)));
  expect(compactConversation).toHaveBeenCalledWith('session', 'preserve tests');
  expect(useChatStore.getState().compactStatus).toEqual({ state: 'done', message: 'Compacted 3 messages, saved ~125 tokens' });
  expect(useProjectStore.getState().contextInfo).toEqual({ tokens: 75, limit: 1000 });
});

it('dispatches slash focus and surfaces the Bridge error without refreshing history', async () => {
  vi.mocked(compactConversation).mockRejectedValue(new Error('Conversation is running'));
  expect(await handleSlashCommand('/compact keep tests', 'session', compact)).toBe(true);
  expect(compactConversation).toHaveBeenCalledWith('session', 'keep tests');
  expect(useChatStore.getState().compactStatus).toEqual({ state: 'error', message: 'Conversation is running' });
  expect(reload).not.toHaveBeenCalled();
  expect(getContextSize).not.toHaveBeenCalled();
});
