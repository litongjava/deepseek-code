import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';

const response = (status: string, database: boolean, ok = true) => ({
  ok, json: async () => ({ status, database }),
});

beforeEach(() => {
  vi.resetModules();
  vi.useFakeTimers();
  window.history.replaceState({}, '', '/');
  delete (window as any).__TAURI_INTERNALS__;
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  delete (window as any).__TAURI_INTERNALS__;
});

describe('backend readiness', () => {
  it('shares one check and waits through connection failures and an unhealthy database', async () => {
    const fetchMock = vi.fn()
      .mockRejectedValueOnce(new TypeError('Failed to fetch'))
      .mockResolvedValueOnce(response('healthy', false))
      .mockResolvedValueOnce(response('healthy', true));
    vi.stubGlobal('fetch', fetchMock);
    const { waitForApiReady } = await import('./bridgeConnection');
    const ready = vi.fn();
    const pending = waitForApiReady();
    expect(waitForApiReady()).toBe(pending);
    void pending.then(ready);
    await vi.advanceTimersByTimeAsync(1500);
    expect(ready).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1500);
    await pending;
    expect(ready).toHaveBeenCalledOnce();
    expect(fetchMock.mock.calls.every(([url]) => url === '/api/health')).toBe(true);
  });

  it('never falls through after a timeout and allows a later retry', async () => {
    const fetchMock = vi.fn().mockResolvedValue(response('starting', false, false));
    vi.stubGlobal('fetch', fetchMock);
    const { waitForApiReady } = await import('./bridgeConnection');
    const failed = expect(waitForApiReady()).rejects.toThrow('等待后端启动超时');
    await vi.advanceTimersByTimeAsync(300_000);
    await failed;
    fetchMock.mockResolvedValue(response('healthy', true));
    await expect(waitForApiReady()).resolves.toBeUndefined();
  });

  it('waits for the native endpoint without probing another healthy instance', async () => {
    (window as any).__TAURI_INTERNALS__ = {};
    vi.mocked(invoke).mockResolvedValueOnce(null).mockResolvedValueOnce([30087, 'own-key']);
    const fetchMock = vi.fn().mockResolvedValue(response('healthy', true));
    vi.stubGlobal('fetch', fetchMock);
    const bridge = await import('./bridgeConnection');
    const pending = bridge.waitForApiReady();
    await vi.advanceTimersByTimeAsync(0);
    expect(fetchMock).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1500);
    await pending;
    expect(bridge.API_BASE).toBe('http://127.0.0.1:30087/api');
    expect(localStorage.getItem('bridge_api_key')).toBe('own-key');
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it('uses only its native endpoint even when another instance is healthy', async () => {
    (window as any).__TAURI_INTERNALS__ = {};
    vi.mocked(invoke).mockResolvedValueOnce([30086, 'test-bridge-key']);
    vi.stubGlobal('fetch', vi.fn(async (url: string) => {
      if (url === 'http://127.0.0.1:30086/health' || url === 'http://127.0.0.1:30085/health') return response('healthy', true);
      throw new TypeError('Failed to fetch');
    }));
    const bridge = await import('./bridgeConnection');
    await bridge.waitForApiReady();
    expect(invoke).toHaveBeenCalledWith('get_bridge_endpoint');
    expect(fetch).toHaveBeenCalledTimes(1);
    expect(fetch).toHaveBeenCalledWith('http://127.0.0.1:30086/health', expect.any(Object));
    expect(await bridge.detectBridgePort()).toBe(30086);
    expect(bridge.API_BASE).toBe('http://127.0.0.1:30086/api');
    expect(localStorage.getItem('bridge_api_key')).toBe('test-bridge-key');
  });
});

describe('dcd-server connection', () => {
  const json = (body: unknown, ok = true, status = 200) => ({ ok, status, json: async () => body });

  beforeEach(() => {
    vi.useRealTimers();
    vi.stubEnv('DEV', false);
    localStorage.clear();
  });
  afterEach(() => vi.unstubAllEnvs());

  it('connects to the serving origin and drops a stale Desktop key when the cookie session is valid', async () => {
    localStorage.setItem('bridge_api_key', 'stale-desktop-key');
    const fetchMock = vi.fn(async (url: string) => url.endsWith('/health')
      ? json({ status: 'healthy', database: true })
      : json({ authenticated: true, server_mode: true }));
    vi.stubGlobal('fetch', fetchMock);
    const bridge = await import('./bridgeConnection');
    expect(bridge.connectionMode()).toBe('web');
    await bridge.waitForApiReady();
    expect(bridge.API_BASE).toBe(`${location.origin}/api`);
    expect(bridge.bridgeOrigin()).toBe(location.origin);
    expect(localStorage.getItem('bridge_api_key')).toBeNull();
    expect(fetchMock).toHaveBeenCalledWith(`${location.origin}/api/auth/status`, expect.not.objectContaining({ credentials: 'include' }));
  });

  it('asks for the access token, logs in, and keeps it for Bearer requests', async () => {
    let authenticated = false;
    const fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/health')) return json({ status: 'healthy', database: true });
      if (url.endsWith('/api/auth/login')) {
        const ok = JSON.parse(String(init?.body)).token === 'right-token';
        authenticated = ok;
        return json(ok ? { ok } : { error: 'Invalid access token' }, ok, ok ? 200 : 401);
      }
      return json({ authenticated, server_mode: true });
    });
    vi.stubGlobal('fetch', fetchMock);
    const bridge = await import('./bridgeConnection');
    const required = vi.fn();
    bridge.onLoginRequired(required);
    await expect(bridge.waitForApiReady()).rejects.toMatchObject({ code: 'needsToken' });
    expect(required).toHaveBeenCalledOnce();
    await expect(bridge.loginToServer('wrong')).rejects.toMatchObject({ code: 'badToken' });
    await bridge.loginToServer(' right-token ');
    expect(localStorage.getItem('bridge_api_key')).toBe('right-token');
    await expect(bridge.waitForApiReady()).resolves.toBeUndefined();
  });

  it('remembers another server, validates its address, and reports an unreachable one', async () => {
    const fetchMock = vi.fn(async (url: string) => {
      if (url.startsWith('https://down.example')) throw new TypeError('Failed to fetch');
      if (url.endsWith('/health')) return json({ status: 'healthy', database: true });
      if (url.endsWith('/api/auth/login')) return json({ ok: true });
      return json({ authenticated: true, server_mode: true });
    });
    vi.stubGlobal('fetch', fetchMock);
    const bridge = await import('./bridgeConnection');
    await expect(bridge.loginToServer('t', 'ftp://bad')).rejects.toMatchObject({ code: 'invalidUrl' });
    await expect(bridge.loginToServer('t', 'https://down.example')).rejects.toMatchObject({ code: 'unreachable' });
    await bridge.loginToServer('token', 'https://dev.example:8443/');
    expect(bridge.savedServerUrl()).toBe('https://dev.example:8443');
    await bridge.waitForApiReady();
    expect(bridge.API_BASE).toBe('https://dev.example:8443/api');
    expect(await bridge.detectBridgePort()).toBe(8443);
  });

  it('returns to the login screen when the server rejects the token', async () => {
    vi.stubGlobal('fetch', vi.fn());
    const bridge = await import('./bridgeConnection');
    localStorage.setItem('dcd_access_token', 'expired');
    const required = vi.fn();
    bridge.onLoginRequired(required);
    bridge.notifyUnauthorized();
    expect(required).toHaveBeenCalledWith(expect.objectContaining({ code: 'expired' }));
    expect(localStorage.getItem('dcd_access_token')).toBeNull();
  });

  it('runs the mobile app on its own engine until a server is chosen', async () => {
    (window as any).__TAURI_INTERNALS__ = {};
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue('Mozilla/5.0 (Linux; Android 14) AppleWebKit');
    vi.mocked(invoke).mockResolvedValueOnce([30085, 'phone-key']);
    vi.stubGlobal('fetch', vi.fn().mockResolvedValue({ ok: true, json: async () => ({ status: 'healthy', database: true }) }));
    const bridge = await import('./bridgeConnection');
    expect(bridge.isMobileApp).toBe(true);
    expect(bridge.connectionMode()).toBe('local');
    expect(bridge.hasNativeDesktop()).toBe(false);
    await bridge.waitForApiReady();
    expect(invoke).toHaveBeenCalledWith('get_bridge_endpoint');
    expect(bridge.API_BASE).toBe('http://127.0.0.1:30085/api');
    expect(localStorage.getItem('bridge_api_key')).toBe('phone-key');

    const required = vi.fn();
    bridge.onLoginRequired(required);
    bridge.requestServerLogin();
    expect(required).toHaveBeenCalledWith(expect.objectContaining({ code: 'needsServer', needsServerUrl: true }));

    localStorage.setItem('dcd_server_url', 'https://server.example');
    expect(bridge.connectionMode()).toBe('web');
    window.location.hash = '/chat/remote-only';
    bridge.switchToLocalEngine();
    expect(bridge.connectionMode()).toBe('local');
    expect(bridge.savedServerUrl()).toBeNull();
    expect(window.location.hash).toBe('#/');
    vi.restoreAllMocks();
  });

  it('clears a mobile conversation only after login to a different engine succeeds', async () => {
    (window as any).__TAURI_INTERNALS__ = {};
    vi.spyOn(navigator, 'userAgent', 'get').mockReturnValue('Mozilla/5.0 (Linux; Android 16) AppleWebKit');
    const fetchMock = vi.fn().mockResolvedValue({ ok: false, status: 401 });
    vi.stubGlobal('fetch', fetchMock);
    const bridge = await import('./bridgeConnection');
    window.location.hash = '/chat/local-only';
    await expect(bridge.loginToServer('wrong', 'https://server.example')).rejects.toMatchObject({ code: 'badToken' });
    expect(window.location.hash).toBe('#/chat/local-only');
    fetchMock.mockResolvedValue({ ok: true, status: 200 });
    await bridge.loginToServer('right', 'https://server.example');
    expect(window.location.hash).toBe('#/');
    window.location.hash = '/chat/remote-only';
    await bridge.loginToServer('renewed', 'https://server.example');
    expect(window.location.hash).toBe('#/chat/remote-only');
    vi.restoreAllMocks();
  });
});
