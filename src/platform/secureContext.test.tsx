import { afterEach, describe, expect, it, vi } from 'vitest';
import { act, render, screen } from '@testing-library/react';
import SecureContextDialog from '../components/SecureContextDialog';
import { requireSecureContext, useSecureContextNotice } from './secureContext';
import { copyToClipboard } from '../utils/clipboard';
import en from '../locales/en.json';

function setSecure(value: boolean) {
  Object.defineProperty(window, 'isSecureContext', { configurable: true, value });
}

afterEach(() => {
  vi.restoreAllMocks();
  delete (window as { isSecureContext?: boolean }).isSecureContext;
  act(() => useSecureContextNotice.getState().close());
});

describe('secure context features', () => {
  it('allows features on HTTPS pages without a dialog', () => {
    setSecure(true);
    render(<SecureContextDialog />);
    expect(requireSecureContext('microphone')).toBe(true);
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('explains the HTTPS requirement for the microphone on HTTP pages', () => {
    setSecure(false);
    render(<SecureContextDialog />);
    let allowed = true;
    act(() => { allowed = requireSecureContext('microphone'); });
    expect(allowed).toBe(false);
    expect(screen.getByRole('alert')).toHaveTextContent(en.secureContext.microphone);
    act(() => screen.getByRole('button', { name: en.common.close }).click());
    expect(screen.queryByRole('alert')).toBeNull();
  });

  it('copies through the fallback on HTTP pages and reports only a failed copy', async () => {
    setSecure(false);
    Object.defineProperty(navigator, 'clipboard', { configurable: true, value: undefined });
    const exec = vi.fn().mockReturnValue(true);
    Object.defineProperty(document, 'execCommand', { configurable: true, value: exec });
    render(<SecureContextDialog />);
    expect(await copyToClipboard('text')).toBe(true);
    expect(exec).toHaveBeenCalledWith('copy');
    expect(screen.queryByRole('alert')).toBeNull();
    exec.mockReturnValue(false);
    await act(async () => { expect(await copyToClipboard('text')).toBe(false); });
    expect(screen.getByRole('alert')).toHaveTextContent(en.secureContext.clipboard);
  });

  it('provides crypto.randomUUID where the browser omits it', async () => {
    const original = crypto.randomUUID;
    Object.defineProperty(crypto, 'randomUUID', { configurable: true, writable: true, value: undefined });
    vi.resetModules();
    await import('./randomUUID');
    const id = crypto.randomUUID();
    expect(id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(crypto.randomUUID()).not.toBe(id);
    Object.defineProperty(crypto, 'randomUUID', { configurable: true, writable: true, value: original });
  });
});
