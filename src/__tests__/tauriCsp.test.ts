import { describe, expect, it } from 'vitest';
import tauriConf from '../../src-tauri/tauri.conf.json';
import tauriWindowsConf from '../../src-tauri/tauri.windows.conf.json';

function directives(csp: string): Map<string, string[]> {
  return new Map(csp.split(';').map(part => part.trim()).filter(Boolean).map(part => {
    const [name, ...sources] = part.split(/\s+/);
    return [name, sources];
  }));
}

describe('Desktop content security policy', () => {
  const csp = directives(tauriConf.app.security.csp);

  it('lets the right-sidebar browser frame local dev servers and https pages', () => {
    const frameSrc = csp.get('frame-src');
    expect(frameSrc).toEqual(expect.arrayContaining(["'self'", 'blob:', 'data:', 'http://localhost:*', 'http://127.0.0.1:*', 'https:']));
    expect(frameSrc).not.toContain('*');
    expect(frameSrc).not.toContain('http:');
  });

  it('keeps the other directives strict', () => {
    expect(csp.get('default-src')).toEqual(["'self'"]);
    expect(csp.get('script-src')).toEqual(["'self'", "'unsafe-inline'", 'blob:']);
    for (const sources of csp.values()) expect(sources).not.toContain('*');
  });

  it('is not overridden by the Windows configuration', () => {
    expect((tauriWindowsConf.app as { security?: unknown }).security).toBeUndefined();
  });
});
