import { afterEach, expect, it, vi } from 'vitest';

afterEach(() => { vi.unstubAllEnvs(); vi.resetModules(); });

it('sends account paths to a configured service and everything else stays on the Bridge', async () => {
  vi.stubEnv('VITE_ACCOUNT_SERVICE_URL', 'https://accounts.example/');
  const account = await import('./accountService');
  expect(account.accountUrl('/user/profile')).toBe('https://accounts.example/api/user/profile');
  expect(account.isAccountRoute('/user/profile')).toBe(true);
  expect(account.isAccountRoute('/github/status')).toBe(false);
  expect(account.isAccountRoute('/conversations')).toBe(false);
});

it('fails immediately when no account service is configured', async () => {
  vi.stubEnv('VITE_ACCOUNT_SERVICE_URL', '');
  const account = await import('./accountService');
  expect(account.ACCOUNT_SERVICE_URL).toBe('');
  expect(() => account.accountUrl('/auth/login')).toThrow(account.AccountServiceUnavailableError);
});
