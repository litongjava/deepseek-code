import { afterEach, describe, expect, it, vi } from 'vitest';
import { sendMessage as sendDesktopMessage, reconnectStream as reconnectDesktop } from '../api';
import { reconnectStream as reconnectModular } from './conversations';
import { sendMessage as sendModularMessage } from './chat';

vi.mock('../services/bridgeConnection', async importOriginal => ({
  ...await importOriginal<typeof import('../services/bridgeConnection')>(),
  API_BASE: '/api', hasBridge: true, waitForApiReady: vi.fn().mockResolvedValue(undefined),
  detectBridgePort: vi.fn().mockResolvedValue(30085),
}));
vi.mock('./client', () => ({
  API_BASE: '/api', isTauriApp: true, getToken: () => null,
  detectBridgePort: vi.fn().mockResolvedValue(30085),
  isBridgeReachable: vi.fn().mockResolvedValue(true), resetBridgeReachableCache: vi.fn(),
  getUserModeForConversation: () => 'selfhosted', resolveEnvCreds: () => ({}),
}));

afterEach(() => vi.unstubAllGlobals());

describe.each([['desktop', sendDesktopMessage], ['modular', sendModularMessage]] as const)('%s streaming text', (_name, sendMessage) => {
  it('forwards terminal turn timing before completion and preserves structured image history', async () => {
    const turn = { turnId: 'turn', startedAt: '2026-11-03T00:00:00Z', endedAt: '2026-11-03T00:00:08Z', elapsedMs: 8000, status: 'completed' };
    const body = [{ type: 'turn_timing', turn: { ...turn, endedAt: null, elapsedMs: null, status: 'running' } }, { type: 'turn_timing', turn }, { type: 'message_stop' }].map(event => `data: ${JSON.stringify(event)}\n\n`).join('');
    const fetchMock = vi.fn().mockResolvedValue(new Response(body));
    vi.stubGlobal('fetch', fetchMock);
    const content = [{ type: 'image_url', image_url: { url: 'data:image/png;base64,aGVsbG8=' } }];
    const events: string[] = [];
    await sendMessage('test', 'image', null, vi.fn(), () => events.push('done'), vi.fn(), undefined, (type, _text, data) => { if (type === 'turn_timing') events.push(data.turn.status); }, undefined, undefined, undefined, undefined, undefined, undefined, 'model', [{ role: 'user', content }]);
    expect(events).toEqual(['running', 'completed', 'done']);
    expect(JSON.parse(fetchMock.mock.calls[0][1].body).messages[0].content).toEqual(content);
  });
  it('forwards request-scoped permission completion before stream termination', async () => {
    const permission = { type: 'tool_permission', request_id: 'approval', tool_use_id: 'write', tool_name: 'Write', input: {} };
    const resolved = { type: 'tool_permission_resolved', request_id: 'approval', tool_use_id: 'write', conversation_id: 'test' };
    const body = [permission, resolved, { type: 'message_stop' }].map(event => `data: ${JSON.stringify(event)}\n\n`).join('');
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(body)));
    const system = vi.fn(), error = vi.fn(), done = vi.fn();
    await sendMessage('test', 'hello', null, vi.fn(), done, error, undefined, system);
    expect(error).not.toHaveBeenCalled();
    expect(system.mock.calls.filter(call => call[0].startsWith('tool_permission')))
      .toEqual([['tool_permission', '', permission], ['tool_permission_resolved', '', resolved]]);
    expect(done).toHaveBeenCalled();
  });
  it('forwards HTTP 400 with its original response body', async () => {
    const body = '{"detail":"The model is not supported."}';
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(body, { status: 400 })));
    const onError = vi.fn();
    await sendMessage('test', 'hello', null, vi.fn(), vi.fn(), onError);
    expect(onError).toHaveBeenCalledWith(`HTTP 400\n${body}`);
  });
  it('forwards a structured 400 error from an established stream', async () => {
    const event = { type: 'error', status: 400, error: { message: 'Unsupported model' } };
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(`data: ${JSON.stringify(event)}\n\n`)));
    const onError = vi.fn();
    const onDone = vi.fn();
    await sendMessage('test', 'hello', null, vi.fn(), onDone, onError);
    expect(onError).toHaveBeenCalledWith('HTTP 400\n{"message":"Unsupported model"}');
    expect(onDone).not.toHaveBeenCalled();
  });
  it('displays short Chinese text immediately, before a delayed tool starts', async () => {
    let controller!: ReadableStreamDefaultController<Uint8Array>;
    const stream = new ReadableStream<Uint8Array>({ start(value) { controller = value; } });
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(stream)));
    const events: string[] = [];
    const error = vi.fn();
    const done = vi.fn();
    const push = (data: unknown) => controller.enqueue(new TextEncoder().encode(`data: ${JSON.stringify(data)}\n\n`));
    const running = sendMessage('test', 'hello', null, (delta) => events.push(`text:${delta}`), done, error,
      undefined, undefined, undefined, undefined, undefined, undefined, event => events.push(`tool:${event.type}`), undefined, 'test-model');
    push({ type: 'content_block_delta', delta: { type: 'text_delta', text: '我查一下。' } });
    await vi.waitFor(() => expect(events).toEqual(['text:我查一下。']));
    push({ type: 'tool_use_start', tool_use_id: 'search', tool_name: 'WebSearch', tool_input: { query: 'test' } });
    push({ type: 'tool_use_done', tool_use_id: 'search', content: 'result' });
    push({ type: 'content_block_delta', delta: { type: 'text_delta', text: '结果如下。' } });
    push({ type: 'message_stop' });
    controller.close();
    await running;
    expect(error).not.toHaveBeenCalled();
    expect(events).toEqual(['text:我查一下。', 'tool:start', 'tool:done', 'text:结果如下。']);
    expect(done).toHaveBeenCalledWith('我查一下。结果如下。');
  });

  it('keeps split artifact tags out of chat while promptly emitting surrounding text', async () => {
    const chunks = ['前言。<cp_', 'artifact title="Report" format="markdown">', '# 内容', '</cp_art', 'ifact>结束。'];
    const body = chunks.map(text => `data: ${JSON.stringify({ type: 'content_block_delta', delta: { type: 'text_delta', text } })}\n\n`).join('')
      + 'data: {"type":"message_stop"}\n\n';
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(body)));
    const done = vi.fn(), error = vi.fn(), draft = vi.fn();
    await sendMessage('test', 'hello', null, vi.fn(), done, error, undefined, undefined, undefined, undefined, draft, undefined, undefined, undefined, 'test-model');
    expect(error).not.toHaveBeenCalled();
    expect(done).toHaveBeenCalledWith('前言。结束。');
    expect(draft).toHaveBeenLastCalledWith(expect.objectContaining({ title: 'Report', preview: '# 内容', done: true }));
  });
});


describe.each([['desktop', reconnectDesktop], ['modular', reconnectModular]] as const)('%s permission reconnect', (_name, reconnect) => {
  it('forwards an already resolved request instead of reopening its approval', async () => {
    const resolved = { type: 'tool_permission_resolved', request_id: 'previous', conversation_id: 'test' };
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue(new Response(`data: ${JSON.stringify(resolved)}\n\ndata: {"type":"message_stop"}\n\n`)));
    const system = vi.fn(), error = vi.fn();
    reconnect('test', vi.fn(), vi.fn(), error, undefined, system);
    await vi.waitFor(() => expect(system).toHaveBeenCalledWith('tool_permission_resolved', '', resolved));
    expect(error).not.toHaveBeenCalled();
  });
});
