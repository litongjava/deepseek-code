import { describe, expect, it } from 'vitest';
import { isLocalNetworkHost, isLoopbackUrl, normalizeBrowserUrl } from './browserUrl';

const APP_ORIGIN = 'http://127.0.0.1:5200';

describe('normalizeBrowserUrl', () => {
  it.each([
    ['localhost:5173', 'http://localhost:5173/'],
    ['  localhost:3000/app?x=1#top ', 'http://localhost:3000/app?x=1#top'],
    ['127.0.0.1:8080', 'http://127.0.0.1:8080/'],
    ['[::1]:4200', 'http://[::1]:4200/'],
    ['app.localhost:8000', 'http://app.localhost:8000/'],
    ['192.168.1.20:3000', 'http://192.168.1.20:3000/'],
    ['10.0.0.5', 'http://10.0.0.5/'],
    ['172.20.1.1:8080', 'http://172.20.1.1:8080/'],
    ['devbox.local:5173', 'http://devbox.local:5173/'],
    ['example.com', 'https://example.com/'],
    ['172.32.0.1', 'https://172.32.0.1/'],
    ['docs.rs/serde', 'https://docs.rs/serde'],
    ['//example.com/x', 'https://example.com/x'],
    ['http://example.com', 'http://example.com/'],
    ['HTTPS://Example.com/Path', 'https://example.com/Path'],
    ['about:blank', 'about:blank'],
    ['ABOUT:BLANK', 'about:blank'],
  ])('accepts %j as %s', (input, url) => {
    expect(normalizeBrowserUrl(input, APP_ORIGIN)).toEqual({ ok: true, url });
  });

  it.each([
    ['', 'empty'],
    ['   ', 'empty'],
    ['javascript:alert(1)', 'unsupported'],
    ['JavaScript:alert(1)', 'unsupported'],
    ['javascript://%0aalert(1)', 'unsupported'],
    ['file:///etc/passwd', 'unsupported'],
    ['data:text/html,<script>alert(1)</script>', 'unsupported'],
    ['chrome://settings', 'unsupported'],
    ['about:config', 'unsupported'],
    ['about:srcdoc', 'unsupported'],
    ['mailto:a@example.com', 'unsupported'],
    ['blob:https://example.com/uuid', 'unsupported'],
    ['tauri://localhost', 'unsupported'],
    ['ftp://example.com', 'unsupported'],
    ['http://', 'invalid'],
    ['exa mple.com', 'invalid'],
    ['http://127.0.0.1:5200/', 'self'],
    ['127.0.0.1:5200/#/settings', 'self'],
    ['http://tauri.localhost/', 'self'],
    ['ipc.localhost', 'self'],
  ])('rejects %j as %s', (input, error) => {
    expect(normalizeBrowserUrl(input, APP_ORIGIN)).toEqual({ ok: false, error });
  });

  it('only treats the exact app origin as self', () => {
    expect(normalizeBrowserUrl('localhost:5200', APP_ORIGIN)).toEqual({ ok: true, url: 'http://localhost:5200/' });
    expect(normalizeBrowserUrl('127.0.0.1:5200', undefined)).toEqual({ ok: true, url: 'http://127.0.0.1:5200/' });
  });

  it('classifies hosts', () => {
    expect(isLocalNetworkHost('192.168.0.1')).toBe(true);
    expect(isLocalNetworkHost('8.8.8.8')).toBe(false);
    expect(isLoopbackUrl('http://localhost:5173/')).toBe(true);
    expect(isLoopbackUrl('http://192.168.0.2:5173/')).toBe(false);
    expect(isLoopbackUrl('https://example.com/')).toBe(false);
    expect(isLoopbackUrl('not a url')).toBe(false);
  });
});
