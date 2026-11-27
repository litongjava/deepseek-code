import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { memoryApi } from './memory';
import { resetBridgePort } from '../services/bridgeConnection';

beforeEach(() => resetBridgePort());
afterEach(() => { vi.unstubAllGlobals(); localStorage.clear(); });

const healthy = () => new Response(JSON.stringify({ status: 'healthy', database: true }), { status: 200 });

describe('memory bridge API', () => {
  it('uses the browser development proxy and forwards the bridge token', async () => {
    localStorage.setItem('bridge_api_key', 'test-bridge-token');
    const fetchMock = vi.fn().mockResolvedValueOnce(healthy()).mockResolvedValue(new Response(JSON.stringify([]), { status: 200 }));
    vi.stubGlobal('fetch', fetchMock);
    await memoryApi.search('preferences', 5);
    expect(fetchMock).toHaveBeenNthCalledWith(1, '/api/health', expect.any(Object));
    expect(fetchMock).toHaveBeenCalledWith('/api/memory/legacy/search', expect.objectContaining({
      method: 'POST', body: JSON.stringify({ query: 'preferences', top_k: 5 }),
      headers: expect.objectContaining({ Authorization: 'Bearer test-bridge-token' }),
    }));
  });

  it('propagates backend failures rather than reporting a successful write', async () => {
    vi.stubGlobal('fetch', vi.fn().mockResolvedValueOnce(healthy()).mockResolvedValue(new Response(JSON.stringify({ error: 'Memory is disabled' }), { status: 500 })));
    await expect(memoryApi.ingest('Remember my preferences', 0.8)).rejects.toThrow('Memory is disabled');
  });
});

describe('scoped file memory API', () => {
  it('sends scope and version and propagates conflicts', async () => {
    const { fileMemoryApi } = await import('./fileMemory');
    const fetchMock = vi.fn().mockResolvedValueOnce(healthy()).mockResolvedValue(new Response(JSON.stringify({ saved: true }), { status: 200 }));
    vi.stubGlobal('fetch', fetchMock);
    await fileMemoryApi.save({ session_id: 'session-1' }, 'test.md', 'Markdown', 'old-hash');
    expect(fetchMock).toHaveBeenCalledWith('/api/memory/ingest', expect.objectContaining({ method: 'POST', body: JSON.stringify({ scope: { session_id: 'session-1' }, name: 'test.md', content: 'Markdown', version: 'old-hash' }) }));
    fetchMock.mockResolvedValueOnce(new Response(JSON.stringify({ error: 'Conflict' }), { status: 409 }));
    await expect(fileMemoryApi.save({ session_id: 'session-1' }, 'test.md', 'Markdown', 'stale')).rejects.toThrow('Conflict');
  });
});
