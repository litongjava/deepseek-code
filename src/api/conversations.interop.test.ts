import { beforeEach, describe, expect, it, vi } from 'vitest';
vi.mock('./client', () => ({ isTauriApp: true, API_BASE: '/api', getToken: () => '', waitForApiReady: async () => {}, request: vi.fn(), getUserModeForConversation: vi.fn(), resolveEnvCreds: vi.fn() }));
import { getConversation } from './conversations';

beforeEach(() => { localStorage.clear(); vi.unstubAllGlobals(); });
describe('SQLite message mapping', () => {
  it('preserves ordered image bytes and backend turn records rather than local duration', async () => {
    const content = [{ type: 'text', text: '[Image #3]' }, { type: 'image_url', image_url: { url: 'data:image/png;base64,aGVsbG8=' } }];
    const turn = { turnId: 'turn', startedAt: '2026-11-03T00:00:00Z', endedAt: '2026-11-03T00:00:08Z', elapsedMs: 8000, status: 'completed' };
    vi.stubGlobal('fetch', vi.fn(async () => new Response(JSON.stringify({ id: 'conversation', messages: [{ id: 'u', role: 'user', content }, { id: 'a', role: 'assistant', content: 'Answer', turn }] }), { status: 200 })));
    const result = await getConversation('conversation');
    expect(result.messages[0].content).toEqual(content);
    expect(result.messages[1].turn).toEqual(turn);
  });
});
