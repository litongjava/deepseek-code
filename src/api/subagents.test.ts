import { beforeEach, describe, expect, it, vi } from 'vitest';
import { request } from './client';
import { getSubagentSnapshot, listSubagents, type SubagentInvocation } from './subagents';
vi.mock('./client', () => ({ request: vi.fn() }));
vi.mock('../api', () => ({ mapHistoryMessage: (message: any) => ({ ...message, toolCalls: message.tool_calls }) }));
export const invocation: SubagentInvocation = { parent_session_id: 'parent', parent_message_id: 'message', tool_use_id: 'tool', child_session_id: 'child', attempt_id: 'attempt', mode: 'foreground', status: 'running', description: 'same name' };
const page = (messages: any[], next_offset: number | null, cursor = 'stable') => ({ invocation, messages, next_offset, cursor, status: 'running', parent_exists: true });
const reply = (data: unknown) => vi.mocked(request).mockResolvedValueOnce({ json: async () => data } as Response);
beforeEach(() => vi.clearAllMocks());
describe('subagent reads', () => {
  it('resolves exact tool identity, never a matching label', async () => {
    reply({ invocations: [invocation, { ...invocation, tool_use_id: 'other' }] });
    expect(await listSubagents('parent', 'tool')).toEqual([invocation]);
    expect(request).toHaveBeenCalledWith('/conversations/parent/subagents?tool_use_id=tool', { signal: undefined });
  });
  it('reads every page and deduplicates stable IDs preserving typed blocks', async () => {
    const typed = { content: [{ type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'aGVsbG8=' } }] };
    reply(page(Array.from({ length: 100 }, (_, i) => ({ id: String(i), content: 'full'.repeat(3000) })), 100));
    reply(page([{ id: '99', content: 'updated' }, { id: '100', tool_calls: [{ id: 't', mcp_result: typed }] }], null));
    const result = await getSubagentSnapshot('parent', 'child', new AbortController().signal);
    expect(result.messages).toHaveLength(101);
    expect(result.messages[0].content).toHaveLength(12000);
    expect(result.messages[99].content).toBe('updated');
    expect(result.messages[100].toolCalls[0].mcp_result).toEqual(typed);
  });
  it('rejects mixed snapshots, unrelated children and stale cancellation', async () => {
    reply(page([{ id: 'one' }], 100)); reply(page([], null, 'changed'));
    await expect(getSubagentSnapshot('parent', 'child', new AbortController().signal)).rejects.toThrow('changed');
    reply({ ...page([], null), invocation: { ...invocation, child_session_id: 'wrong' } });
    await expect(getSubagentSnapshot('parent', 'child', new AbortController().signal)).rejects.toThrow('identity');
    const controller = new AbortController(); controller.abort();
    await expect(getSubagentSnapshot('parent', 'child', controller.signal)).rejects.toThrow();
  });
});
