import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useBusySendStore } from './useBusySendStore';
import { queueAPI, type QueueItem } from '../api/queue';
vi.mock('../api/queue', () => ({ queueAPI: { enqueue: vi.fn(), list: vi.fn(), mutate: vi.fn(), resume: vi.fn() } }));
const item = (overrides: Partial<QueueItem> = {}): QueueItem => ({ id: 1, sessionId: 'a', requestId: 'r', revision: 1, content: 'hello', mode: 'next_turn', state: 'queued', ...overrides });
beforeEach(() => { vi.resetAllMocks(); localStorage.clear(); useBusySendStore.setState({ queues: {} }); });
describe('durable queue projection', () => {
  it('retains failed content and retries with the same request id', async () => {
    vi.mocked(queueAPI.enqueue).mockRejectedValueOnce(new Error('offline')).mockImplementationOnce(async (session, p) => item({ ...p, sessionId: session }));
    await useBusySendStore.getState().enqueue('a', 'draft', 'next_turn');
    const pending = useBusySendStore.getState().queues.a.pending[0];
    expect(pending).toMatchObject({ status: 'failed', content: 'draft', error: 'offline' });
    expect(localStorage.getItem('durable-queue-outbox-v1')).toContain(pending.requestId);
    await useBusySendStore.getState().retry('a', pending.requestId);
    expect(queueAPI.enqueue).toHaveBeenLastCalledWith('a', { requestId: pending.requestId, content: 'draft', mode: 'next_turn' });
    expect(useBusySendStore.getState().queues.a.pending).toEqual([]);
  });
  it('does not resurrect event-before-ack handoff or leak across sessions', async () => {
    let ack!: (value: QueueItem) => void;
    vi.mocked(queueAPI.enqueue).mockImplementation(() => new Promise(resolve => { ack = resolve; }));
    const sending = useBusySendStore.getState().enqueue('a', 'hello', 'steer');
    const requestId = useBusySendStore.getState().queues.a.pending[0].requestId;
    useBusySendStore.getState().accept('a', item({ requestId, revision: 3, state: 'handoff' }));
    useBusySendStore.getState().accept('b', item());
    ack(item({ requestId }));
    await sending;
    expect(useBusySendStore.getState().queues.a.items[0].state).toBe('handoff');
    expect(useBusySendStore.getState().queues.a.pending).toEqual([]);
    expect(useBusySendStore.getState().queues.b).toBeUndefined();
  });
  it('rejects an older list response after mutation acknowledgement', async () => {
    useBusySendStore.getState().accept('a', item());
    let list!: (value: any) => void;
    vi.mocked(queueAPI.list).mockImplementation(() => new Promise(resolve => { list = resolve; }));
    const refresh = useBusySendStore.getState().refresh('a');
    useBusySendStore.getState().accept('a', item({ revision: 2, state: 'cancelled' }));
    list({ items: [item()], paused: false });
    await refresh;
    expect(useBusySendStore.getState().queues.a.items[0].state).toBe('cancelled');
  });
  it('reconciles a lost ack against terminal snapshot without resending', async () => {
    vi.mocked(queueAPI.enqueue).mockRejectedValue(new Error('lost ack'));
    await useBusySendStore.getState().enqueue('a', 'hello', 'next_turn');
    const requestId = useBusySendStore.getState().queues.a.pending[0].requestId;
    vi.mocked(queueAPI.list).mockResolvedValue({ items: [item({ requestId, state: 'handoff', revision: 3 })], paused: true });
    await useBusySendStore.getState().refresh('a');
    expect(useBusySendStore.getState().queues.a.pending).toEqual([]);
    expect(useBusySendStore.getState().queues.a.paused).toBe(true);
    expect(queueAPI.enqueue).toHaveBeenCalledTimes(1);
  });
  it('restores unacknowledged image content after a module reload', async () => {
    localStorage.setItem('durable-queue-outbox-v1', JSON.stringify({ a: [{ requestId: 'reload', content: [{ type: 'image_url', image_url: { url: 'data:image/png;base64,YWJj' } }], mode: 'next_turn', status: 'sending' }] }));
    vi.resetModules();
    const restored = (await import('./useBusySendStore')).useBusySendStore.getState().queues.a;
    expect(restored.pending[0]).toMatchObject({ requestId: 'reload', status: 'failed', content: [{ type: 'image_url' }] });
    expect(restored.pending[0].error).toContain('not confirmed');
  });
  it('keeps authoritative consumed state and reports edit conflict', async () => {
    useBusySendStore.getState().accept('a', item());
    vi.mocked(queueAPI.mutate).mockRejectedValue(new Error('revision conflict'));
    vi.mocked(queueAPI.list).mockResolvedValue({ items: [item({ revision: 2, state: 'claimed' })], paused: false });
    expect(await useBusySendStore.getState().mutate('a', item(), 'edit', 'draft')).toBe(false);
    expect(useBusySendStore.getState().queues.a).toMatchObject({ error: 'revision conflict', busy: [], items: [{ state: 'claimed' }] });
  });
});
