import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import * as legacyApi from '../api';
import { fsCreate, fsDelete, fsRead, fsTree, fsWrite } from './fs';
import { resetBridgePort } from '../services/bridgeConnection';

beforeEach(() => {
  resetBridgePort();
  localStorage.setItem('bridge_api_key', 'bridge-key');
});
afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); });

const healthy = () => new Response(JSON.stringify({ status: 'healthy', database: true }), { status: 200 });
const ok = (body: unknown) => new Response(JSON.stringify(body), { status: 200 });

function stubFetch(response: () => Response = () => ok({ success: true, path: '/w', tree: [], content: '' })) {
  const fetchMock = vi.fn(async (url: string, _init?: RequestInit) => (url === '/api/health' ? healthy() : response()));
  vi.stubGlobal('fetch', fetchMock);
  return fetchMock;
}

const authorized = expect.objectContaining({ headers: expect.objectContaining({ Authorization: 'Bearer bridge-key' }) });

// Both the legacy src/api.ts helpers and the src/api/fs.ts module talk to the Bridge.
const variants = [
  { name: 'src/api.ts', tree: legacyApi.getFileSystemTree, read: legacyApi.readFileContent, write: legacyApi.writeFileContent, create: legacyApi.createFileOrDir, remove: legacyApi.deleteFileOrDir },
  { name: 'src/api/fs.ts', tree: fsTree, read: fsRead, write: fsWrite, create: fsCreate, remove: fsDelete },
];

describe.each(variants)('filesystem Bridge helpers ($name)', ({ tree, read, write, create, remove }) => {
  it('sends the Bridge key on every filesystem request', async () => {
    const fetchMock = stubFetch();
    await tree('/w/repo', 1);
    await tree();
    await read('/w/repo/a b.txt', { maxBytes: 1024 });
    await read('/w/repo/a.txt');
    await write('/w/repo/a.txt', 'x');
    await create('/w/repo/dir', true);
    await remove('/w/repo/dir');

    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/tree?path=%2Fw%2Frepo&depth=1', authorized);
    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/tree', authorized);
    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/read?path=%2Fw%2Frepo%2Fa+b.txt&max_bytes=1024', authorized);
    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/read?path=%2Fw%2Frepo%2Fa.txt', authorized);
    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/write', expect.objectContaining({
      method: 'POST', body: JSON.stringify({ path: '/w/repo/a.txt', content: 'x' }),
      headers: expect.objectContaining({ Authorization: 'Bearer bridge-key', 'Content-Type': 'application/json' }),
    }));
    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/create', expect.objectContaining({ method: 'POST', body: JSON.stringify({ path: '/w/repo/dir', is_dir: true }) }));
    expect(fetchMock).toHaveBeenCalledWith('/api/filesystem/delete', expect.objectContaining({ method: 'DELETE', body: JSON.stringify({ path: '/w/repo/dir' }) }));
    const fsCalls = fetchMock.mock.calls.filter(([url]) => url !== '/api/health');
    expect(fsCalls).toHaveLength(7);
    for (const [, init] of fsCalls) expect(init).toEqual(authorized);
  });

  it('keeps the "<action>: <status>" error messages', async () => {
    stubFetch(() => new Response('', { status: 404 }));
    await expect(tree('/missing', 1)).rejects.toThrow('Failed to get file tree: 404');
    await expect(read('/missing')).rejects.toThrow('Failed to read file: 404');
    await expect(write('/missing', '')).rejects.toThrow('Failed to write file: 404');
    await expect(create('/missing')).rejects.toThrow('Failed to create: 404');
    await expect(remove('/missing')).rejects.toThrow('Failed to delete: 404');
  });
});
