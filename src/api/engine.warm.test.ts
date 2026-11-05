import { afterEach, expect, it, vi } from 'vitest';

vi.mock('../services/bridgeConnection', () => ({ isTauriApp: false, useDevProxy: true, waitForApiReady: vi.fn().mockResolvedValue(undefined), API_BASE: '/api', detectBridgePort: vi.fn(), resetBridgePort: vi.fn() }));

afterEach(() => { localStorage.clear(); vi.unstubAllGlobals(); });
it.each(['desktop', 'modular'])('%s navigation does not submit stale saved permission authority', async implementation => {
  localStorage.setItem('permission_mode', 'bypass_permissions');
  const fetch = vi.fn().mockResolvedValue({ ok: true, status: 200, json: async () => ({ ok: true }) });
  vi.stubGlobal('fetch', fetch);
  const api = implementation === 'desktop' ? await import('../api') : await import('./engine');
  await api.warmEngine('pending-plan');
  await vi.waitFor(() => expect(fetch).toHaveBeenCalled());
  const warm = fetch.mock.calls.find(call => String(call[0]).includes('/pending-plan/warm'));
  expect(warm).toBeTruthy();
  expect(JSON.parse(warm![1].body)).not.toHaveProperty('permission_mode');
});
