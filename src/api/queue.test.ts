import { expect, it, vi, beforeEach } from 'vitest';
import { queueAPI, type QueueItem } from './queue';
import { request } from './client';
vi.mock('./client', () => ({ request: vi.fn() }));
beforeEach(() => vi.resetAllMocks());
it('maps revision-sensitive operations to the real session scoped routes', async () => {
  vi.mocked(request).mockResolvedValue({ json: async () => ({}) } as Response);
  const item = { id: 12, revision: 4 } as QueueItem;
  await queueAPI.mutate('a/b', item, 'edit', 'draft');
  await queueAPI.mutate('a/b', item, 'delete');
  await queueAPI.mutate('a/b', item, 'steer');
  expect(vi.mocked(request).mock.calls).toEqual([
    ['/conversations/a%2Fb/queue/12', { method: 'PATCH', body: '{"revision":4,"text":"draft"}' }],
    ['/conversations/a%2Fb/queue/12', { method: 'DELETE', body: '{"revision":4}' }],
    ['/conversations/a%2Fb/queue/12/steer', { method: 'POST', body: '{"revision":4}' }],
  ]);
});
it('preserves backend failure instead of faking a successful delete', async () => {
  vi.mocked(request).mockRejectedValue(new Error('conflict'));
  await expect(queueAPI.mutate('a', { id: 1, revision: 2 } as QueueItem, 'delete')).rejects.toThrow('conflict');
});
