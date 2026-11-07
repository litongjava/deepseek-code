import { act, fireEvent, render, screen, within } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import CustomizePage from './CustomizePage';
import * as api from '../api';
import { customizationApi } from '../api/customization';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';

vi.mock('../api', () => ({
  getSkills: vi.fn(), getSkillDetail: vi.fn(), getSkillFile: vi.fn(),
  createSkill: vi.fn(), updateSkill: vi.fn(), deleteSkill: vi.fn(), toggleSkill: vi.fn(),
  getGithubStatus: vi.fn(async () => ({ connected: false })),
}));
vi.mock('../api/customization', () => ({ customizationApi: { load: vi.fn(), save: vi.fn() } }));
vi.mock('./MarkdownRenderer', () => ({ default: ({ content }: { content: string }) => <p>{content}</p>, CodeBlock: () => null }));
vi.mock('./McpSettingsPage', () => ({ default: () => <h2>MCP management</h2> }));
vi.mock('./PluginsPage', () => ({ default: () => <h2>Plugin management</h2> }));

const skill = { id: 'skill_1', name: 'review', description: 'Review changes', content: 'Actual instructions', enabled: false, source: 'user', isExample: false, sourceDir: '/skills/review', files: [] };
const nav = () => within(screen.getByRole('navigation', { name: en.customize.title }));
async function openSkills() {
  await act(async () => { render(<MemoryRouter><CustomizePage /></MemoryRouter>); });
  await act(async () => fireEvent.click(nav().getByRole('button', { name: 'Skills' })));
}
beforeEach(() => {
  vi.clearAllMocks();
  useUIStore.getState().setLanguage('en');
  vi.mocked(api.getSkills).mockResolvedValue({ skills: [skill] });
  vi.mocked(api.getSkillDetail).mockResolvedValue(skill);
  vi.mocked(customizationApi.load).mockResolvedValue({
    customization: { system_prompt: { mode: 'default', text: '' }, disabled_tools: [], tool_descriptions: {}, custom_tools: [] },
    default_system_prompt: 'BUILT IN',
    builtin_tools: [{ name: 'Write', description: 'Writes files' }],
  } as never);
  vi.mocked(customizationApi.save).mockResolvedValue();
});

describe('Customize management', () => {
  // Both entries reuse one component instance, so a draft left on the other entry
  // must never be committed by this entry's save.
  it('keeps an unsaved prompt out of a later tool save', async () => {
    await act(async () => { render(<MemoryRouter><CustomizePage /></MemoryRouter>); });
    await act(async () => fireEvent.click(nav().getByRole('button', { name: 'Prompts' })));
    fireEvent.change(await screen.findByLabelText(en.agentSettings.modeLabel), { target: { value: 'append' } });
    fireEvent.change(screen.getByLabelText(en.agentSettings.promptText), { target: { value: 'UNSAVED PROMPT' } });
    expect(screen.getByText(en.agentSettings.save)).not.toBeDisabled();

    await act(async () => fireEvent.click(nav().getByRole('button', { name: 'Tools' })));
    fireEvent.click(await screen.findByLabelText(en.agentSettings.enableTool.replace('{name}', 'Write')));
    await act(async () => fireEvent.click(screen.getByText(en.agentSettings.save)));

    expect(customizationApi.save).toHaveBeenCalledTimes(1);
    const sent = vi.mocked(customizationApi.save).mock.calls[0][0];
    expect(sent.system_prompt).toEqual({ mode: 'default', text: '' });
    expect(sent.disabled_tools).toEqual(['Write']);
  });

  it('keeps an empty replacement prompt on the prompt entry from blocking a tool save', async () => {
    await act(async () => { render(<MemoryRouter><CustomizePage /></MemoryRouter>); });
    await act(async () => fireEvent.click(nav().getByRole('button', { name: 'Prompts' })));
    fireEvent.change(await screen.findByLabelText(en.agentSettings.modeLabel), { target: { value: 'replace' } });
    fireEvent.change(screen.getByLabelText(en.agentSettings.promptText), { target: { value: '  ' } });
    expect(screen.getByText(en.agentSettings.save)).toBeDisabled();

    await act(async () => fireEvent.click(nav().getByRole('button', { name: 'Tools' })));
    fireEvent.click(await screen.findByLabelText(en.agentSettings.enableTool.replace('{name}', 'Write')));
    const save = screen.getByText(en.agentSettings.save);
    expect(save).not.toBeDisabled();
    await act(async () => fireEvent.click(save));
    expect(vi.mocked(customizationApi.save).mock.calls[0][0].system_prompt).toEqual({ mode: 'default', text: '' });
  });

  it('shares ordered entries and card styles, and opens management rather than creation', async () => {
    await act(async () => { render(<MemoryRouter><CustomizePage /></MemoryRouter>); });
    const names = ['Skills', 'MCP', 'Connectors', 'Plugins', 'Prompts', 'Tools'];
    expect(nav().getAllByRole('button').map(button => button.textContent)).toEqual(names);
    const cards = within(screen.getByLabelText(en.customize.manageContext)).getAllByRole('button');
    expect(cards.map(button => button.querySelector('.font-medium')?.textContent)).toEqual(names);
    expect(new Set(cards.map(button => button.className)).size).toBe(1);
    await act(async () => fireEvent.click(cards[0]));
    expect(screen.getByText('review')).toBeInTheDocument();
    expect(screen.queryByRole('textbox', { name: 'Name' })).not.toBeInTheDocument();
    expect(nav().getByRole('button', { name: 'Skills' })).toHaveAttribute('aria-current', 'page');
    fireEvent.click(screen.getByRole('button', { name: 'Customize' }));
    fireEvent.click(within(screen.getByLabelText(en.customize.manageContext)).getAllByRole('button')[1]);
    expect(screen.getByRole('heading', { name: 'MCP management' })).toBeInTheDocument();
    fireEvent.click(nav().getByRole('button', { name: 'MCP' }));
    expect(screen.getByRole('heading', { name: 'MCP management' })).toBeInTheDocument();
  });

  it('uses real list/detail fields and preserves disabled state on toggle failure', async () => {
    vi.mocked(api.getSkills).mockResolvedValue({ skills: [skill, { ...skill, id: 'bundled', name: 'bundled-skill', source: 'bundled' }] });
    await openSkills();
    expect(screen.getByText('Examples')).toBeInTheDocument();
    await act(async () => fireEvent.click(screen.getByText('review')));
    expect(screen.getByText('Actual instructions')).toBeInTheDocument();
    const toggle = screen.getByRole('switch');
    expect(toggle).toHaveAttribute('aria-checked', 'false');
    vi.mocked(api.toggleSkill).mockRejectedValueOnce(new Error('Write denied'));
    await act(async () => fireEvent.click(screen.getByRole('switch')));
    expect(screen.getByRole('alert')).toHaveTextContent('Write denied');
    expect(toggle).toHaveAttribute('aria-checked', 'false');
    vi.mocked(api.toggleSkill).mockResolvedValueOnce({ ok: true });
    vi.mocked(api.getSkillDetail).mockResolvedValueOnce({ ...skill, enabled: true, content: 'Persisted enabled instructions' });
    await act(async () => fireEvent.click(screen.getByRole('switch')));
    expect(screen.getByRole('switch')).toHaveAttribute('aria-checked', 'true');
  });

  it('shows list errors rather than an empty success and supports retry', async () => {
    vi.mocked(api.getSkills).mockRejectedValueOnce(new Error('Bridge unavailable'));
    await openSkills();
    expect(screen.getByRole('alert')).toHaveTextContent('Bridge unavailable');
    expect(screen.queryByText(en.customize.noSkillsFound)).not.toBeInTheDocument();
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Refresh' })));
    expect(screen.getByText('review')).toBeInTheDocument();
  });

  it('shows detail failures and allows selecting the same skill to retry', async () => {
    vi.mocked(api.getSkillDetail).mockRejectedValueOnce(new Error('Detail unavailable'));
    await openSkills();
    await act(async () => fireEvent.click(screen.getByText('review')));
    expect(screen.getByRole('alert')).toHaveTextContent('Detail unavailable');
    await act(async () => fireEvent.click(screen.getByText('review')));
    expect(screen.getByText('Actual instructions')).toBeInTheDocument();
    expect(screen.queryByRole('alert')).not.toBeInTheDocument();
  });

  it('creates with returned identity and enabled state, and saves using persisted detail', async () => {
    await openSkills();
    fireEvent.click(screen.getByRole('button', { name: 'Create new skills' }));
    fireEvent.click(screen.getByRole('button', { name: 'Write skill instructions' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Name' }), { target: { value: 'new-skill' } });
    vi.mocked(api.createSkill).mockRejectedValueOnce(new Error('Already exists'));
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Create Skill' })));
    expect(screen.getByRole('alert')).toHaveTextContent('Already exists');
    const created = { ...skill, id: 'new-id', name: 'new-skill', content: 'Persisted content' };
    vi.mocked(api.createSkill).mockResolvedValueOnce(created);
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Create Skill' })));
    expect(screen.getByRole('heading', { name: 'new-skill' })).toBeInTheDocument();
    expect(screen.getByRole('switch')).toHaveAttribute('aria-checked', 'false');
    fireEvent.click(screen.getByRole('button', { name: 'Edit details' }));
    fireEvent.change(screen.getByRole('textbox', { name: 'Content' }), { target: { value: 'Updated instructions' } });
    vi.mocked(api.updateSkill).mockRejectedValueOnce(new Error('Save denied'));
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Save' })));
    expect(screen.getByRole('alert')).toHaveTextContent('Save denied');
    expect(screen.getByRole('textbox', { name: 'Content' })).toHaveValue('Updated instructions');
    vi.mocked(api.updateSkill).mockResolvedValueOnce({ ok: true });
    vi.mocked(api.getSkillDetail).mockResolvedValueOnce({ ...created, content: 'Canonical persisted instructions' });
    await act(async () => fireEvent.click(screen.getByRole('button', { name: 'Save' })));
    expect(api.updateSkill).toHaveBeenLastCalledWith('new-id', { name: 'new-skill', description: '', content: 'Updated instructions' });
    expect(screen.getByText('Canonical persisted instructions')).toBeInTheDocument();
  });
});
