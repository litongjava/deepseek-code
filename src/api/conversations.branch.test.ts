import { beforeEach, describe, expect, it, vi } from 'vitest';
vi.mock('./client', () => ({ isTauriApp: false, API_BASE: '', getToken: () => '', waitForApiReady: async () => {}, request: vi.fn(), getUserModeForConversation: vi.fn(), resolveEnvCreds: vi.fn() }));
import { branchConversation } from './conversations';
import { createLocalConversation, getLocalConversation, getLocalConversations, getLocalMessages, saveLocalMessages, updateLocalConversation } from '../services/localStorageService';

beforeEach(() => localStorage.clear());
describe('conversation branches', () => {
  function source() {
    const conversation = createLocalConversation('Original', 'model', { workspace_mode: 'code' });
    updateLocalConversation(conversation.id, { workspace_path: 'D:/project', provider: 'provider', project_id: 'project' });
    saveLocalMessages(conversation.id, ['user', 'assistant', 'user', 'assistant'].map((role, index) => ({
      id: `m${index}`, conversation_id: conversation.id, role, content: `message ${index}`, sort_order: index,
      created_at: '2026-10-14T00:00:00Z', requestSnapshots: role === 'assistant' ? [{ id: `r${index}`, number: 1, model: 'model', startedAt: 1, systemPrompt: null, systemMessages: [], tools: [], inputTokens: 20, outputTokens: 10 }] : [],
    })));
    return conversation;
  }
  it('includes the selected answer, preserves context and leaves source history intact', async () => {
    const original = source();
    const branch = await branchConversation(original.id, 'm1');
    const messages = getLocalMessages(branch.new_conversation_id);
    expect(messages.map(message => message.content)).toEqual(['message 0', 'message 1']);
    expect(messages[1].id).not.toBe('m1');
    expect(messages[1].requestSnapshots?.[0].outputTokens).toBe(10);
    expect(getLocalConversation(branch.new_conversation_id)).toMatchObject({ workspace_mode: 'code', workspace_path: 'D:/project', provider: 'provider', project_id: 'project', message_count: 2 });
    expect(getLocalMessages(original.id)).toHaveLength(4);
  });
  it('keeps embedded images while giving copied turns independent non-running identities', async () => {
    const original = source();
    const content = [{ type: 'text', text: '[Image #8]' }, { type: 'image_url', image_url: { url: 'data:image/png;base64,aGVsbG8=' } }];
    const turn = { turnId: 'running-source', startedAt: '2026-11-03T00:00:00Z', endedAt: null, elapsedMs: null, status: 'running' as const };
    saveLocalMessages(original.id, getLocalMessages(original.id).map((message, index) => index === 0 ? { ...message, content } : index === 1 ? { ...message, turn } : message));
    const branch = await branchConversation(original.id, 'm1');
    const copied = getLocalMessages(branch.new_conversation_id);
    expect(copied[0].content).toEqual(content);
    expect(copied[1].turn).toMatchObject({ status: 'cancelled', elapsedMs: null, endedAt: null });
    expect(copied[1].turn?.turnId).not.toBe(turn.turnId);
    expect(getLocalMessages(original.id)[1].turn).toEqual(turn);
  });
  it('uses the clicked position when optimistic IDs differ, and rejects missing boundaries', async () => {
    const original = source();
    const branch = await branchConversation(original.id, 'optimistic-id', 1);
    expect(getLocalMessages(branch.new_conversation_id)).toHaveLength(2);
    await expect(branchConversation(original.id, 'missing')).rejects.toThrow('Branch message not found');
    await expect(branchConversation(original.id, undefined, 99)).rejects.toThrow('Branch message not found');
    expect(getLocalConversations()).toHaveLength(2);
  });
});
