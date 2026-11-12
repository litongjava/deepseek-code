import { beforeEach, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { invoke } from '@tauri-apps/api/core';
import McpSettingsPage from './McpSettingsPage';

beforeEach(() => vi.resetAllMocks());
it('preserves imported arguments, working directory and disabled state when edited', async () => {
  const server = { id: 'old', name: 'old', command: 'node', args: ['D:/path with spaces/server.cjs', '', 'quoted "text"'], cwd: 'D:/work space', env: {}, enabled: false, running: false, tools_count: 0, resources_count: 0, transport_type: 'stdio' };
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers') return [server];
    if (cmd === 'mcp_list_tools') return [];
    if (cmd === 'mcp_get_states') return {};
  });
  render(<McpSettingsPage />);
  fireEvent.click(await screen.findByTitle('Edit'));
  expect(screen.getByLabelText('MCP 工作目录')).toHaveValue(server.cwd);
  fireEvent.click(screen.getByRole('button', { name: 'Update' }));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith('mcp_update_server', { id: 'old', config: { id: 'old', name: 'old', command: 'node', args: server.args, cwd: server.cwd, env: {}, enabled: false, type: 'stdio' } }));
});

it('keeps the URL and headers of a remote server when edited', async () => {
  const server = { id: 'docs', name: 'docs', command: '', args: [], env: {}, enabled: true, running: false, tools_count: 0, resources_count: 0, transport_type: 'http', url: 'https://mcp.example.com/mcp', headers: { Authorization: 'Bearer t' } };
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers') return [server];
    if (cmd === 'mcp_list_tools') return [];
    if (cmd === 'mcp_get_states') return {};
  });
  render(<McpSettingsPage />);
  fireEvent.click(await screen.findByTitle('Edit'));
  expect(screen.getByLabelText('Transport Type')).toHaveValue('http');
  expect(screen.getByLabelText('URL')).toHaveValue(server.url);
  fireEvent.click(screen.getByRole('button', { name: 'Update' }));
  await waitFor(() => expect(invoke).toHaveBeenCalledWith('mcp_update_server', { id: 'docs', config: { id: 'docs', name: 'docs', command: '', cwd: null, args: [], type: 'http', url: server.url, headers: server.headers, env: {}, enabled: true } }));
});

it('adds a WebSocket server with its URL', async () => {
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers') return [];
    if (cmd === 'mcp_list_tools') return [];
    if (cmd === 'mcp_get_states') return {};
  });
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


it.each(['Start', 'Stop', 'Restart', 'Delete'])('refreshes servers and tools after %s', async action => {
  let running = action === 'Stop' || action === 'Restart';
  const enabled = true;
  let removed = false;
  const confirm = vi.spyOn(window, 'confirm').mockReturnValue(true);
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers') return removed ? [] : [{ id: 'fixture', name: 'Fixture', command: 'node', args: [], enabled, running, tools_count: running ? 1 : 0, resources_count: 0, transport_type: 'stdio' }];
    if (cmd === 'mcp_list_tools') return running && enabled && !removed ? [{ name: 'fixture-tool', description: '', server_name: 'Fixture' }] : [];
    if (cmd === 'mcp_get_states') return {};
    if (cmd === 'mcp_start_server') running = true;
    if (cmd === 'mcp_stop_server') running = false;
    if (cmd === 'mcp_remove_server') removed = true;
  });
  render(<McpSettingsPage />);
  await screen.findByTitle('Edit');
  const callsBefore = vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === 'mcp_list_tools').length;
  fireEvent.click(screen.getByTitle(action));
  await waitFor(() => expect(vi.mocked(invoke).mock.calls.filter(([cmd]) => cmd === 'mcp_list_tools').length).toBeGreaterThan(callsBefore));
  if (running && enabled && !removed) expect(await screen.findByText('Available MCP Tools (1)')).toBeInTheDocument();
  else await waitFor(() => expect(screen.queryByText('Available MCP Tools (1)')).not.toBeInTheDocument());
  confirm.mockRestore();
});

it('shows list and tools failures instead of empty success and retries them', async () => {
  let fail = true;
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers' || cmd === 'mcp_list_tools') {
      if (fail) throw new Error('IPC unavailable');
      return [];
    }
    if (cmd === 'mcp_get_states') return {};
  });
  render(<McpSettingsPage />);
  expect(await screen.findByRole('alert')).toHaveTextContent('Failed to load MCP servers');
  expect(screen.getByRole('alert')).toHaveTextContent('Failed to load MCP tools');
  expect(screen.queryByText('No MCP servers configured')).not.toBeInTheDocument();
  fail = false;
  fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
  expect(await screen.findByText('No MCP servers configured')).toBeInTheDocument();
  expect(screen.queryByRole('alert')).not.toBeInTheDocument();
});

it('keeps the form and exposes native mutation errors', async () => {
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers' || cmd === 'mcp_list_tools') return [];
    if (cmd === 'mcp_get_states') return {};
    if (cmd === 'mcp_add_server') throw new Error('Configuration is read-only');
  });
  render(<McpSettingsPage />);
  fireEvent.click(await screen.findByRole('button', { name: 'Add Server' }));
  fireEvent.change(screen.getByPlaceholderText('my-mcp-server'), { target: { value: 'fixture' } });
  fireEvent.change(screen.getByPlaceholderText('npx'), { target: { value: 'node' } });
  fireEvent.click(screen.getByRole('button', { name: 'Add' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('Configuration is read-only');
  expect(screen.getByPlaceholderText('my-mcp-server')).toHaveValue('fixture');
  expect(screen.queryByText('Server added')).not.toBeInTheDocument();
});

it("shows each server's state in a selector", async () => {
  const servers = [
    { id: 'a', name: 'Alpha', command: 'node', args: [], env: {}, enabled: true, running: true, tools_count: 2, resources_count: 0, transport_type: 'stdio' },
    { id: 'b', name: 'Beta', command: 'node', args: [], env: {}, enabled: false, running: false, tools_count: 0, resources_count: 0, transport_type: 'stdio' },
  ];
  vi.mocked(invoke).mockImplementation(async cmd => {
    if (cmd === 'mcp_list_servers') return servers;
    if (cmd === 'mcp_list_tools') return [];
    if (cmd === 'mcp_get_states') return { a: 'on-demand', b: 'off' };
  });
  render(<McpSettingsPage />);
  expect(await screen.findByLabelText('MCP state Alpha')).toHaveValue('on-demand');
  expect(screen.getByLabelText('MCP state Beta')).toHaveValue('off');
  expect(screen.getByText('Tools are listed by name and loaded with ToolSearch when needed.')).toBeInTheDocument();
  expect(screen.getByText('Not started; its tools and prompts are unavailable.')).toBeInTheDocument();
});

it('sets a state through mcp_set_state and refreshes', async () => {
  const server = { id: 'a', name: 'Alpha', command: 'node', args: [], env: {}, enabled: true, running: false, tools_count: 0, resources_count: 0, transport_type: 'stdio' };
  let state = 'user';
  let stateLoads = 0;
  vi.mocked(invoke).mockImplementation(async (cmd, args) => {
    if (cmd === 'mcp_list_servers') return [server];
    if (cmd === 'mcp_list_tools') return [];
    if (cmd === 'mcp_get_states') {
      stateLoads += 1;
      return { a: state };
    }
    if (cmd === 'mcp_set_state') {
      state = String((args as { state: string }).state);
      return state;
    }
  });
  render(<McpSettingsPage />);
  const select = await screen.findByLabelText('MCP state Alpha');
  const loadsBefore = stateLoads;
  fireEvent.change(select, { target: { value: 'user-only' } });
  await waitFor(() => expect(invoke).toHaveBeenCalledWith('mcp_set_state', { id: 'a', state: 'user-only' }));
  expect(await screen.findByText('State set to user-only')).toBeInTheDocument();
  await waitFor(() => expect(stateLoads).toBeGreaterThan(loadsBefore));
  await waitFor(() => expect(screen.getByLabelText('MCP state Alpha')).toHaveValue('user-only'));
});
