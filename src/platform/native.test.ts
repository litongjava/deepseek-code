import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';

vi.unmock('../platform/native');
const request = vi.hoisted(() => vi.fn());
vi.mock('../api/client', () => ({ request }));

beforeEach(() => { vi.resetModules(); vi.mocked(invoke).mockClear(); request.mockReset(); });
afterEach(() => { delete (window as any).__TAURI_INTERNALS__; vi.restoreAllMocks(); });

it('calls the Tauri command in the Desktop app', async () => {
  (window as any).__TAURI_INTERNALS__ = {};
  vi.mocked(invoke).mockResolvedValueOnce('native');
  const { invokeDesktop } = await import('./native');
  await expect(invokeDesktop('list_slash_commands')).resolves.toBe('native');
  expect(invoke).toHaveBeenCalledWith('list_slash_commands', undefined, undefined);
});

it('serves peripheral requests through the Bridge in a browser', async () => {
  request.mockResolvedValue({ json: async () => ({ started: true }) });
  const { invokeDesktop } = await import('./native');
  await expect(invokeDesktop('peripheral_command', { request: { action: 'learning_status' } })).resolves.toEqual({ started: true });
  expect(request).toHaveBeenCalledWith('/peripheral', { method: 'POST', body: JSON.stringify({ request: { action: 'learning_status' } }) });
  expect(invoke).not.toHaveBeenCalled();
});

it('reports Desktop-only commands clearly in a browser', async () => {
  const { invokeDesktop, DesktopOnlyError, hasWebFallback } = await import('./native');
  await expect(invokeDesktop('show_item_in_folder', { path: '/x' })).rejects.toBeInstanceOf(DesktopOnlyError);
  expect(hasWebFallback('show_item_in_folder')).toBe(false);
  expect(invoke).not.toHaveBeenCalled();
});

it('opens external links in a new tab without Tauri', async () => {
  const open = vi.spyOn(window, 'open').mockReturnValue(null);
  const { invokeDesktop } = await import('./native');
  await invokeDesktop('open_external_url', { url: 'https://example.com' });
  expect(open).toHaveBeenCalledWith('https://example.com', '_blank', 'noopener,noreferrer');
});

it('sends byte payloads to Desktop as raw IPC bodies with headers', async () => {
  (window as any).__TAURI_INTERNALS__ = {};
  vi.mocked(invoke).mockResolvedValueOnce('text');
  const { invokeDesktop } = await import('./native');
  const audio = new Uint8Array([1, 2, 3]);
  const options = { headers: { 'x-voice-id': 'job', 'x-voice-mime': 'audio/webm' } };
  await expect(invokeDesktop('voice_transcribe', audio, options)).resolves.toBe('text');
  expect(invoke).toHaveBeenCalledWith('voice_transcribe', audio, options);
});

it('never passes byte payloads to a JSON web fallback', async () => {
  const { invokeDesktop, DesktopOnlyError } = await import('./native');
  await expect(invokeDesktop('peripheral_command', new Uint8Array([1]))).rejects.toBeInstanceOf(DesktopOnlyError);
  expect(request).not.toHaveBeenCalled();
});
