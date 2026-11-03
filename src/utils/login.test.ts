import { afterEach, expect, it, vi } from 'vitest';

afterEach(() => { vi.unstubAllEnvs(); vi.resetModules(); });

it('defaults to local use and keeps an unauthorized request on its current page', async () => {
  vi.stubEnv('VITE_ENABLE_LOGIN', undefined);
  const { loginEnabled, handleUnauthorized } = await import('./login');
  expect(loginEnabled).toBe(false);
  window.location.hash = '#/chat/local';
  localStorage.setItem('auth_token', 'expired');
  handleUnauthorized();
  expect(window.location.hash).toBe('#/chat/local');
  expect(localStorage.getItem('auth_token')).toBeNull();
});

it('retains login as an explicitly enabled feature', async () => {
  vi.stubEnv('VITE_ENABLE_LOGIN', 'true');
  expect((await import('./login')).loginEnabled).toBe(true);
});
