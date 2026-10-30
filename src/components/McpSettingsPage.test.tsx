import { beforeEach, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { invoke } from '@tauri-apps/api/core';
import McpSettingsPage from './McpSettingsPage';

beforeEach(() => vi.resetAllMocks());
it('preserves imported arguments, working directory and disabled state when edited', async () => {
  const server = { id: 'old', name: 'old', command: 'node', args: ['D:/path with spaces/server.cjs', '', 'quoted "text"'], cwd: 'D:/work space', env: {}, enabled: false, running: false, tools_count: 0, resources_count: 0, transport_type: 'stdio' };
  vi.mocked(invoke).mockImplementation(async cmd => cmd === 'mcp_list_servers' ? [server] : cmd === 'mcp_list_tools' ? [] : undefined);
  render(<McpSettingsPage />);
  fireEvent.click(await screen.findByTitle('Edit'));
  expect(screen.getByLabelText('MCP 工作目录')).toHaveValue(server.cwd);
  fireEvent.click(screen.getByRole('button', { name: 'Update' }));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith('mcp_update_server', { id: 'old', config: { id: 'old', name: 'old', command: 'node', args: server.args, cwd: server.cwd, env: {}, enabled: false, type: 'stdio' } }));
});

it('keeps the URL and headers of a remote server when edited', async () => {
  const server = { id: 'docs', name: 'docs', command: '', args: [], env: {}, enabled: true, running: false, tools_count: 0, resources_count: 0, transport_type: 'http', url: 'https://mcp.example.com/mcp', headers: { Authorization: 'Bearer t' } };
  vi.mocked(invoke).mockImplementation(async cmd => cmd === 'mcp_list_servers' ? [server] : cmd === 'mcp_list_tools' ? [] : undefined);
  render(<McpSettingsPage />);
  fireEvent.click(await screen.findByTitle('Edit'));
  expect(screen.getByLabelText('Transport Type')).toHaveValue('http');
  expect(screen.getByLabelText('URL')).toHaveValue(server.url);
  fireEvent.click(screen.getByRole('button', { name: 'Update' }));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith('mcp_update_server', { id: 'docs', config: { id: 'docs', name: 'docs', command: '', cwd: null, args: [], type: 'http', url: server.url, headers: server.headers, env: {}, enabled: true } }));
});

it('adds a WebSocket server with its URL', async () => {
  vi.mocked(invoke).mockImplementation(async cmd => cmd === 'mcp_list_servers' ? [] : cmd === 'mcp_list_tools' ? [] : undefined);
  render(<McpSettingsPage />);
  fireEvent.click(await screen.findByRole('button', { name: /Add Server/ }));
  fireEvent.change(screen.getByPlaceholderText('my-mcp-server'), { target: { value: 'live' } });
  fireEvent.change(screen.getByLabelText('Transport Type'), { target: { value: 'ws' } });
  fireEvent.click(screen.getByRole('button', { name: 'Add' }));
  expect(await screen.findByText('Name and URL are required')).toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('URL'), { target: { value: 'wss://mcp.example.com/ws' } });
  fireEvent.click(screen.getByRole('button', { name: 'Add' }));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith('mcp_add_server', { config: { id: 'live', name: 'live', command: '', cwd: null, args: [], type: 'ws', url: 'wss://mcp.example.com/ws', headers: {}, env: {}, enabled: true } }));
});
