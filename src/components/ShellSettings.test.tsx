import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import { open } from '@tauri-apps/plugin-dialog';
import { getShellSettings, saveShellSettings, getWslDistributions } from '../api';
import { useUIStore } from '../stores/useUIStore';
import ShellSettings from './ShellSettings';

vi.mock('../api', () => ({ getShellSettings: vi.fn(), saveShellSettings: vi.fn(), getWslDistributions: vi.fn() }));
vi.mock('@tauri-apps/plugin-dialog', () => ({ open: vi.fn() }));
vi.mock('../services/bridgeConnection', () => ({ isTauriApp: true }));

const status = {
  settings: { toolDefault: 'bash', terminalDefault: 'auto', bashPath: null, wsl: { distribution: null, bashPath: '/bin/bash' } },
  tool: { program: 'D:\\Git\\bin\\bash.exe', source: 'detected' },
  terminal: { program: 'powershell.exe', source: 'detected' },
};

beforeEach(() => {
  vi.resetAllMocks();
  useUIStore.getState().setLanguage('en');
  vi.mocked(getShellSettings).mockResolvedValue(structuredClone(status));
  vi.mocked(saveShellSettings).mockImplementation(async settings => ({ ...status, settings }));
});

it('loads shared settings, selects a file and reads back saved values', async () => {
  vi.mocked(open).mockResolvedValue('D:\\Program Files\\Git\\bin\\bash.exe');
  render(<ShellSettings />);
  expect(await screen.findByLabelText('Bash executable path')).toHaveValue('');
  fireEvent.click(screen.getByRole('button', { name: 'Choose executable' }));
  await waitFor(() => expect(screen.getByLabelText('Bash executable path')).toHaveValue('D:\\Program Files\\Git\\bin\\bash.exe'));
  fireEvent.change(screen.getByLabelText('Manual terminal shell'), { target: { value: 'bash' } });
  fireEvent.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByText('Saved. New commands and terminals use these settings.')).toBeInTheDocument();
  expect(saveShellSettings).toHaveBeenCalledWith({ ...status.settings, terminalDefault: 'bash', bashPath: 'D:\\Program Files\\Git\\bin\\bash.exe' });
});

it('requires explicit WSL selection and preserves edits after a failed save', async () => {
  vi.mocked(getWslDistributions).mockResolvedValue(['docker-desktop', 'dcl-linux-test']);
  vi.mocked(saveShellSettings).mockRejectedValue(new Error('Invalid Bash path'));
  render(<ShellSettings />);
  fireEvent.change(await screen.findByLabelText('Model command shell'), { target: { value: 'wsl' } });
  fireEvent.click(screen.getByRole('button', { name: 'Find WSL distributions' }));
  await screen.findByRole('option', { name: 'dcl-linux-test', hidden: true });
  expect(screen.getByLabelText('WSL distribution')).toHaveValue('');
  fireEvent.change(screen.getByLabelText('WSL distribution'), { target: { value: 'dcl-linux-test' } });
  fireEvent.click(screen.getByRole('button', { name: 'Save' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Invalid Bash path');
  expect(screen.getByLabelText('WSL distribution')).toHaveValue('dcl-linux-test');
  expect(screen.queryByText('Saved. New commands and terminals use these settings.')).not.toBeInTheDocument();
});

it('shows load errors without offering to overwrite the configuration', async () => {
  vi.mocked(getShellSettings).mockRejectedValue(new Error('Invalid settings.json'));
  render(<ShellSettings />);
  expect(await screen.findByRole('alert')).toHaveTextContent('Invalid settings.json');
  expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();
});
