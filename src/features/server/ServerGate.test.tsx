import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';

const json = (body: unknown, ok = true, status = 200) => ({ ok, status, json: async () => body });

beforeEach(() => {
  vi.resetModules();
  vi.stubEnv('DEV', false);
  localStorage.clear();
  localStorage.setItem('app_language', 'en');
});
afterEach(() => { vi.unstubAllEnvs(); vi.unstubAllGlobals(); });

it('shows the token login for a dcd-server, then the app after a successful sign-in', async () => {
  let authenticated = false;
  vi.stubGlobal('fetch', vi.fn(async (url: string, init?: RequestInit) => {
    if (url.endsWith('/health')) return json({ status: 'healthy', database: true });
    if (url.endsWith('/api/auth/login')) {
      authenticated = JSON.parse(String(init?.body)).token === 'right';
      return authenticated ? json({ ok: true }) : json({}, false, 401);
    }
    return json({ authenticated, server_mode: true });
  }));
  const { default: ServerGate } = await import('./ServerGate');
  render(<ServerGate><p>App content</p></ServerGate>);
  const token = await screen.findByLabelText('Access token');
  expect(screen.queryByLabelText('Server address')).not.toBeInTheDocument();

  fireEvent.change(token, { target: { value: 'wrong' } });
  fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('The access token is incorrect.');

  fireEvent.change(token, { target: { value: 'right' } });
  fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
  expect(await screen.findByText('App content')).toBeInTheDocument();
});

it('lets the user enter another server address', async () => {
  vi.stubGlobal('fetch', vi.fn(async (url: string) => url.endsWith('/health')
    ? json({ status: 'healthy', database: true })
    : json({ authenticated: false, server_mode: true })));
  const { default: ServerGate } = await import('./ServerGate');
  render(<ServerGate><p>App content</p></ServerGate>);
  fireEvent.click(await screen.findByRole('button', { name: 'Use another server' }));
  expect(screen.getByLabelText('Server address')).toBeInTheDocument();
});

it('passes straight through on the development proxy', async () => {
  vi.stubEnv('DEV', true);
  const { default: ServerGate } = await import('./ServerGate');
  render(<ServerGate><p>App content</p></ServerGate>);
  await waitFor(() => expect(screen.getByText('App content')).toBeInTheDocument());
});

it('names an unreachable server without showing internal codes', async () => {
  vi.stubGlobal('fetch', vi.fn(async (url: string) => {
    if (url.startsWith('https://down.example')) throw new TypeError('Failed to fetch');
    return url.endsWith('/health') ? json({ status: 'healthy', database: true }) : json({ authenticated: false, server_mode: true });
  }));
  const { default: ServerGate } = await import('./ServerGate');
  render(<ServerGate><p>App content</p></ServerGate>);
  fireEvent.click(await screen.findByRole('button', { name: 'Use another server' }));
  fireEvent.change(screen.getByLabelText('Server address'), { target: { value: 'https://down.example' } });
  fireEvent.change(screen.getByLabelText('Access token'), { target: { value: 'token' } });
  fireEvent.click(screen.getByRole('button', { name: 'Sign in' }));
  const alert = await screen.findByRole('alert');
  expect(alert).toHaveTextContent('Cannot reach the server. https://down.example');
  expect(alert.textContent).not.toMatch(/unreachable|needsServer/);
});
