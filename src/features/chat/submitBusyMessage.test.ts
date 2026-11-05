import { beforeEach, expect, it, vi } from 'vitest';
import { submitBusyMessage } from './submitBusyMessage';
import { useBusySendStore } from '../../stores/useBusySendStore';
import type { PendingFile } from '../../components/FileUploadPreview';

beforeEach(() => vi.restoreAllMocks());
it('sends next-turn and steer as backend admission modes, never stop/resend', async () => {
  const enqueue = vi.spyOn(useBusySendStore.getState(), 'enqueue').mockResolvedValue();
  await submitBusyMessage('a', 'next', [], 'queue');
  await submitBusyMessage('b', 'direction', [], 'steer');
  expect(enqueue.mock.calls).toEqual([['a', 'next', 'next_turn'], ['b', 'direction', 'steer']]);
});
it('reads image bytes before releasing composer ownership', async () => {
  const enqueue = vi.spyOn(useBusySendStore.getState(), 'enqueue').mockResolvedValue();
  await submitBusyMessage('a', 'image', [{ file: new File(['abc'], 'x.png', { type: 'image/png' }), fileType: 'image', mimeType: 'image/png', fileName: 'x.png', status: 'done' } as PendingFile], 'queue');
  expect(enqueue).toHaveBeenCalledWith('a', [{ type: 'text', text: 'image' }, { type: 'image_url', image_url: { url: 'data:image/png;base64,YWJj' }, fileName: 'x.png', size: 3 }], 'next_turn');
});
it('does not admit incomplete attachments', async () => {
  const enqueue = vi.spyOn(useBusySendStore.getState(), 'enqueue').mockResolvedValue();
  await expect(submitBusyMessage('a', 'draft', [{ status: 'uploading' } as PendingFile], 'queue')).rejects.toThrow('finish uploading');
  expect(enqueue).not.toHaveBeenCalled();
});
