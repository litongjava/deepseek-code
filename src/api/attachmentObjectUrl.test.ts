import { afterEach, expect, it, vi } from 'vitest';

vi.mock('../services/bridgeConnection', async importOriginal => ({
  ...await importOriginal<typeof import('../services/bridgeConnection')>(),
  API_BASE: 'https://server.example/api',
  waitForApiReady: vi.fn().mockResolvedValue(undefined),
}));

afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); });

it('loads attachments with the Bridge credential instead of a URL token', async () => {
  localStorage.setItem('bridge_api_key', 'bridge-key');
  const fetchMock = vi.fn().mockResolvedValue({ ok: true, blob: async () => new Blob(['png']) });
  vi.stubGlobal('fetch', fetchMock);
  const createObjectURL = vi.fn().mockReturnValue('blob:attachment');
  vi.stubGlobal('URL', Object.assign(URL, { createObjectURL }));
  const { fetchAttachmentObjectUrl } = await import('../api');
  await expect(fetchAttachmentObjectUrl('file-1')).resolves.toBe('blob:attachment');
  expect(fetchMock).toHaveBeenCalledWith('https://server.example/api/uploads/file-1/raw', expect.objectContaining({
    headers: { Authorization: 'Bearer bridge-key' },
  }));
});

it('reports a rejected attachment request', async () => {
  vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: false, status: 401 }));
  const { fetchAttachmentObjectUrl } = await import('../api');
  await expect(fetchAttachmentObjectUrl('file-1')).rejects.toThrow('401');
});
