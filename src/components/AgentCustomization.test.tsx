import { beforeEach, describe, expect, it, vi } from 'vitest';
import { useState } from 'react';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { AgentCustomization } from './AgentCustomization';
import { customizationApi } from '../api/customization';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';
import zh from '../locales/zh.json';

vi.mock('../api/customization', () => ({ customizationApi: { load: vi.fn(), save: vi.fn() } }));

const empty = { system_prompt: { mode: 'default', text: '' }, disabled_tools: [], tool_descriptions: {}, custom_tools: [] };

beforeEach(() => {
  vi.resetAllMocks();
  useUIStore.getState().setLanguage('zh');
  vi.mocked(customizationApi.load).mockResolvedValue({
    customization: JSON.parse(JSON.stringify(empty)),
    default_system_prompt: 'BUILT IN PROMPT',
    builtin_tools: [{ name: 'Read', description: 'Reads a file' }, { name: 'Write', description: 'Writes a file' }],
  } as never);
  vi.mocked(customizationApi.save).mockResolvedValue();
});

const saved = () => vi.mocked(customizationApi.save).mock.calls[0][0];
const named = (template: string, name: string) => template.replace('{name}', name);

describe('prompt settings page', () => {
  it('starts a replacement from the built-in prompt and refuses an empty one', async () => {
    render(<AgentCustomization section="prompts" />);
    const mode = await screen.findByLabelText(zh.agentSettings.modeLabel);
    expect(screen.getByText(zh.agentSettings.save)).toBeDisabled();
    fireEvent.change(mode, { target: { value: 'replace' } });
    const text = screen.getByLabelText(zh.agentSettings.promptText);
    expect(text).toHaveValue('BUILT IN PROMPT');
    fireEvent.change(text, { target: { value: '  ' } });
    expect(screen.getByText(zh.agentSettings.save)).toBeDisabled();
    fireEvent.change(text, { target: { value: 'MY PROMPT' } });
    fireEvent.click(screen.getByText(zh.agentSettings.save));
    await waitFor(() => expect(saved().system_prompt).toEqual({ mode: 'replace', text: 'MY PROMPT' }));
    await screen.findByRole('status');
    expect(screen.getByText(zh.agentSettings.save)).toBeDisabled();
  });

  it('shows only the prompt section', async () => {
    render(<AgentCustomization section="prompts" />);
    await screen.findByLabelText(zh.agentSettings.modeLabel);
    expect(screen.getByRole('heading', { name: zh.customize.prompts })).toBeInTheDocument();
    expect(screen.queryByText(zh.agentSettings.builtinTools)).not.toBeInTheDocument();
    expect(screen.queryByLabelText(zh.agentSettings.toolName)).not.toBeInTheDocument();
  });
});

describe('tool settings page', () => {
  it('turns built-in tools off and edits and restores their descriptions', async () => {
    render(<AgentCustomization section="tools" />);
    fireEvent.click(await screen.findByLabelText(named(zh.agentSettings.enableTool, 'Write')));
    fireEvent.click(screen.getByLabelText(named(zh.agentSettings.editToolDescription, 'Read')));
    const description = screen.getByLabelText(named(zh.agentSettings.descriptionLabel, 'Read'));
    expect(description).toHaveValue('Reads a file');
    fireEvent.change(description, { target: { value: 'Edited' } });
    expect(screen.getByText(zh.agentSettings.modified)).toBeInTheDocument();
    fireEvent.click(screen.getByText(zh.agentSettings.save));
    await waitFor(() => expect(saved()).toMatchObject({ disabled_tools: ['Write'], tool_descriptions: { Read: 'Edited' } }));
    fireEvent.click(screen.getByText(zh.agentSettings.restoreDescription));
    expect(screen.getByLabelText(named(zh.agentSettings.descriptionLabel, 'Read'))).toHaveValue('Reads a file');
    expect(screen.queryByText(zh.agentSettings.modified)).not.toBeInTheDocument();
  });

  it('adds, edits, disables and removes a custom tool', async () => {
    render(<AgentCustomization section="tools" />);
    fireEvent.click(await screen.findByText(zh.agentSettings.addTool));
    fireEvent.change(screen.getByLabelText(zh.agentSettings.toolName), { target: { value: 'lookup' } });
    fireEvent.change(screen.getByLabelText(zh.agentSettings.toolDescription), { target: { value: 'Find an order' } });
    fireEvent.change(screen.getByLabelText(zh.agentSettings.toolCommand), { target: { value: 'cat' } });
    fireEvent.change(screen.getByLabelText(zh.agentSettings.inputSchema), { target: { value: 'not json' } });
    fireEvent.click(screen.getByText(zh.agentSettings.confirm));
    expect(await screen.findByRole('alert')).toHaveTextContent(zh.agentSettings.invalidSchema);
    fireEvent.change(screen.getByLabelText(zh.agentSettings.inputSchema), { target: { value: '{"type":"object"}' } });
    fireEvent.change(screen.getByLabelText(zh.agentSettings.timeout), { target: { value: '30' } });
    fireEvent.click(screen.getByText(zh.agentSettings.confirm));
    fireEvent.click(screen.getByLabelText(named(zh.agentSettings.enableTool, 'lookup')));
    fireEvent.click(screen.getByText(zh.agentSettings.save));
    await waitFor(() => expect(saved().custom_tools).toEqual([{ name: 'lookup', description: 'Find an order', input_schema: { type: 'object' }, command: 'cat', enabled: false, timeout_secs: 30 }]));
    fireEvent.click(screen.getByLabelText(named(zh.agentSettings.editTool, 'lookup')));
    expect(screen.getByLabelText(zh.agentSettings.toolCommand)).toHaveValue('cat');
    fireEvent.click(screen.getByText(zh.agentSettings.cancel));
    fireEvent.click(screen.getByLabelText(named(zh.agentSettings.deleteTool, 'lookup')));
    expect(screen.queryByText('lookup')).not.toBeInTheDocument();
  });

  it('shows only the tool sections', async () => {
    render(<AgentCustomization section="tools" />);
    await screen.findByText(zh.agentSettings.builtinTools);
    expect(screen.getByRole('heading', { name: zh.customize.tools })).toBeInTheDocument();
    expect(screen.queryByLabelText(zh.agentSettings.modeLabel)).not.toBeInTheDocument();
  });

  it('shows the server\'s reason when saving fails', async () => {
    vi.mocked(customizationApi.save).mockRejectedValue(new Error('Read is already a built-in tool'));
    render(<AgentCustomization section="tools" />);
    fireEvent.click(await screen.findByLabelText(named(zh.agentSettings.enableTool, 'Write')));
    fireEvent.click(screen.getByText(zh.agentSettings.save));
    expect(await screen.findByRole('alert')).toHaveTextContent('already a built-in');
  });
});

describe('interface language', () => {
  it('uses the English labels when English is selected', async () => {
    useUIStore.getState().setLanguage('en');
    render(<AgentCustomization section="prompts" />);
    expect(await screen.findByLabelText(en.agentSettings.modeLabel)).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: en.customize.prompts })).toBeInTheDocument();
    expect(screen.getByText(en.agentSettings.modeDefault)).toBeInTheDocument();
  });
});

// The two entries render one component instance, so switching sections keeps the draft.
function Entries() {
  const [section, setSection] = useState<'prompts' | 'tools'>('prompts');
  return <>
    <button onClick={() => setSection('tools')}>to-tools</button>
    <button onClick={() => setSection('prompts')}>to-prompts</button>
    <AgentCustomization section={section} />
  </>;
}

describe('shared draft between entries', () => {
  it('keeps an unsaved prompt after the tool entry saves', async () => {
    render(<Entries />);
    fireEvent.change(await screen.findByLabelText(zh.agentSettings.modeLabel), { target: { value: 'append' } });
    fireEvent.change(screen.getByLabelText(zh.agentSettings.promptText), { target: { value: 'DRAFT PROMPT' } });
    fireEvent.click(screen.getByText('to-tools'));
    fireEvent.click(await screen.findByLabelText(named(zh.agentSettings.enableTool, 'Write')));
    fireEvent.click(screen.getByText(zh.agentSettings.save));
    await waitFor(() => expect(saved()).toMatchObject({ disabled_tools: ['Write'], system_prompt: { mode: 'default', text: '' } }));

    fireEvent.click(screen.getByText('to-prompts'));
    expect(await screen.findByLabelText(zh.agentSettings.promptText)).toHaveValue('DRAFT PROMPT');
    expect(screen.getByText(zh.agentSettings.save)).not.toBeDisabled();
  });

  it('does not submit tool edits from the prompt entry', async () => {
    render(<Entries />);
    fireEvent.click(await screen.findByText('to-tools'));
    fireEvent.click(await screen.findByLabelText(named(zh.agentSettings.enableTool, 'Write')));
    fireEvent.click(screen.getByText('to-prompts'));
    fireEvent.change(await screen.findByLabelText(zh.agentSettings.modeLabel), { target: { value: 'append' } });
    fireEvent.change(screen.getByLabelText(zh.agentSettings.promptText), { target: { value: 'MY PROMPT' } });
    fireEvent.click(screen.getByText(zh.agentSettings.save));
    await waitFor(() => expect(saved().system_prompt).toEqual({ mode: 'append', text: 'MY PROMPT' }));
    expect(saved().disabled_tools).toEqual([]);
  });
});
