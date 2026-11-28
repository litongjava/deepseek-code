import { beforeEach, describe, expect, it, vi } from 'vitest';
vi.mock('./client', () => ({ isTauriApp: true, API_BASE: '/api', getToken: () => '', waitForApiReady: async () => {}, request: vi.fn(), getUserModeForConversation: vi.fn(), resolveEnvCreds: vi.fn() }));
vi.mock('../services/bridgeConnection', async importOriginal => ({
  ...await importOriginal<typeof import('../services/bridgeConnection')>(),
  waitForApiReady: async () => {},
  bridgeOrigin: () => 'http://bridge.test',
}));
import { createConversation, getConversation } from './conversations';
import { getLocalConversations } from '../services/localStorageService';
import { pushToCloud } from '../services/syncService';
import { modelKey } from '../utils/modelConfig';

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

it('pushes the decoded binding from the local mirror after a Bridge picker creation', async () => {
  const provider = 'team/a::b%';
  const model = 'first::shared[1m]';
  const send = vi.spyOn(globalThis, 'fetch')
    .mockResolvedValueOnce(Response.json({ id: 'bridge-chat' }))
    .mockResolvedValueOnce(Response.json({ serverTimestamp: '2026-11-28T00:00:00Z' }));
  try {
    await createConversation('Mirror', modelKey(provider, model));
    expect(getLocalConversations()).toMatchObject([{ provider, model }]);

    localStorage.setItem('auth_token', 'fixture');
    await pushToCloud();
    const [url, request] = send.mock.calls[1];
    expect(String(url)).toMatch(/\/data\/sync\/push$/);
    expect(JSON.parse(String(request?.body)).conversations).toMatchObject([{ provider, model }]);
  } finally {
    send.mockRestore();
  }
});
