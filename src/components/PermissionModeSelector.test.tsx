import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { invoke } from '@tauri-apps/api/core';
import PermissionModeSelector from './PermissionModeSelector';
import { useChatStore } from '../stores/useChatStore';
import { sessionBashPermission } from '../api';

vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }));
vi.mock('../api', () => ({ sessionBashPermission: vi.fn() }));

beforeEach(() => {
  vi.resetAllMocks();
  localStorage.clear();
  useChatStore.getState().setPermissionMode('ask_permissions');
});

it('reads and revokes Bash permission for the selected conversation', async () => {
  vi.mocked(sessionBashPermission).mockResolvedValueOnce({ allowed: true }).mockResolvedValueOnce({ allowed: false });
  render(<PermissionModeSelector conversationId="current-session" />);
  fireEvent.click(screen.getByRole('button', { name: 'Ask permissions' }));
  fireEvent.click(await screen.findByRole('button', { name: 'Revoke session Bash permission' }));
  await waitFor(() => expect(screen.getByText('Session Bash: Not granted')).toBeInTheDocument());
  expect(sessionBashPermission).toHaveBeenCalledWith('current-session', true);
});

it('keeps the current mode when the backend rejects bypass and displays the reason', async () => {
  localStorage.setItem('permission_bypass_confirmed', 'true');
  vi.mocked(invoke).mockRejectedValue('Start Desktop with --allow-dangerously-skip-permissions to enable bypass');
  render(<PermissionModeSelector />);
  fireEvent.click(screen.getByRole('button', { name: 'Ask permissions' }));
  fireEvent.click(screen.getByRole('button', { name: /Bypass permissions/ }));
  expect(await screen.findByRole('alert')).toHaveTextContent('--allow-dangerously-skip-permissions');
  expect(useChatStore.getState().permissionMode).toBe('ask_permissions');
  expect(localStorage.getItem('permission_mode')).toBe('ask_permissions');
});

it('changes the label only after backend confirmation', async () => {
  localStorage.setItem('permission_bypass_confirmed', 'true');
  let resolve!: () => void;
  vi.mocked(invoke).mockImplementation(() => new Promise<void>(done => { resolve = done; }) as any);
  render(<PermissionModeSelector />);
  fireEvent.click(screen.getByRole('button', { name: 'Ask permissions' }));
  fireEvent.click(screen.getByRole('button', { name: /Bypass permissions/ }));
  expect(useChatStore.getState().permissionMode).toBe('ask_permissions');
  expect(screen.getByRole('button', { name: /Bypass permissions/ })).toBeDisabled();
  resolve();
  await waitFor(() => expect(useChatStore.getState().permissionMode).toBe('bypass_permissions'));
  expect(invoke).toHaveBeenCalledWith('set_permission_mode', { mode: 'bypass_permissions' });
});
