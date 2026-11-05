import { beforeEach, describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { AgentCustomization } from './AgentCustomization';
import { customizationApi } from '../api/customization';

vi.mock('../api/customization', () => ({ customizationApi: { load: vi.fn(), save: vi.fn() } }));

const empty = { system_prompt: { mode: 'default', text: '' }, disabled_tools: [], tool_descriptions: {}, custom_tools: [] };

beforeEach(() => {
  vi.resetAllMocks();
  vi.mocked(customizationApi.load).mockResolvedValue({
    customization: JSON.parse(JSON.stringify(empty)),
    default_system_prompt: 'BUILT IN PROMPT',
    builtin_tools: [{ name: 'Read', description: 'Reads a file' }, { name: 'Write', description: 'Writes a file' }],
  } as never);
  vi.mocked(customizationApi.save).mockResolvedValue();
});

const saved = () => vi.mocked(customizationApi.save).mock.calls[0][0];

describe('prompt and tool settings', () => {
  it('starts a replacement from the built-in prompt and refuses an empty one', async () => {
    render(<AgentCustomization />);
    const mode = await screen.findByLabelText('系统提示词使用方式');
    expect(screen.getByText('保存')).toBeDisabled();
    fireEvent.change(mode, { target: { value: 'replace' } });
    const text = screen.getByLabelText('系统提示词内容');
    expect(text).toHaveValue('BUILT IN PROMPT');
    fireEvent.change(text, { target: { value: '  ' } });
    expect(screen.getByText('保存')).toBeDisabled();
    fireEvent.change(text, { target: { value: 'MY PROMPT' } });
    fireEvent.click(screen.getByText('保存'));
    await waitFor(() => expect(saved().system_prompt).toEqual({ mode: 'replace', text: 'MY PROMPT' }));
    await screen.findByRole('status');
    expect(screen.getByText('保存')).toBeDisabled();
  });

  it('turns built-in tools off and edits and restores their descriptions', async () => {
    render(<AgentCustomization />);
    fireEvent.click(await screen.findByLabelText('启用 Write'));
    fireEvent.click(screen.getByLabelText('修改 Read 的说明'));
    const description = screen.getByLabelText('Read 的说明');
    expect(description).toHaveValue('Reads a file');
    fireEvent.change(description, { target: { value: 'Edited' } });
    expect(screen.getByText('已修改')).toBeInTheDocument();
    fireEvent.click(screen.getByText('保存'));
    await waitFor(() => expect(saved()).toMatchObject({ disabled_tools: ['Write'], tool_descriptions: { Read: 'Edited' } }));
    fireEvent.click(screen.getByText('恢复默认说明'));
    expect(screen.getByLabelText('Read 的说明')).toHaveValue('Reads a file');
    expect(screen.queryByText('已修改')).not.toBeInTheDocument();
  });

  it('adds, edits, disables and removes a custom tool', async () => {
    render(<AgentCustomization />);
    fireEvent.click(await screen.findByText('添加工具'));
    fireEvent.change(screen.getByLabelText('工具名称'), { target: { value: 'lookup' } });
    fireEvent.change(screen.getByLabelText('工具说明'), { target: { value: 'Find an order' } });
    fireEvent.change(screen.getByLabelText('工具命令'), { target: { value: 'cat' } });
    fireEvent.change(screen.getByLabelText('输入参数 JSON Schema'), { target: { value: 'not json' } });
    fireEvent.click(screen.getByText('确定'));
    expect(await screen.findByRole('alert')).toHaveTextContent('不是有效的 JSON');
    fireEvent.change(screen.getByLabelText('输入参数 JSON Schema'), { target: { value: '{"type":"object"}' } });
    fireEvent.change(screen.getByLabelText('超时秒数'), { target: { value: '30' } });
    fireEvent.click(screen.getByText('确定'));
    fireEvent.click(screen.getByLabelText('启用 lookup'));
    fireEvent.click(screen.getByText('保存'));
    await waitFor(() => expect(saved().custom_tools).toEqual([{ name: 'lookup', description: 'Find an order', input_schema: { type: 'object' }, command: 'cat', enabled: false, timeout_secs: 30 }]));
    fireEvent.click(screen.getByLabelText('编辑 lookup'));
    expect(screen.getByLabelText('工具命令')).toHaveValue('cat');
    fireEvent.click(screen.getByText('取消'));
    fireEvent.click(screen.getByLabelText('删除 lookup'));
    expect(screen.queryByText('lookup')).not.toBeInTheDocument();
  });

  it('shows the server\'s reason when saving fails', async () => {
    vi.mocked(customizationApi.save).mockRejectedValue(new Error('Read is already a built-in tool'));
    render(<AgentCustomization />);
    fireEvent.click(await screen.findByLabelText('启用 Write'));
    fireEvent.click(screen.getByText('保存'));
    expect(await screen.findByRole('alert')).toHaveTextContent('already a built-in');
  });
});
