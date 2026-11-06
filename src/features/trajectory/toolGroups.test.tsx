import { fireEvent, render, screen, within } from '@testing-library/react';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import { groupTitle, groupWorkSteps, messageSteps } from './model';
import WorkDetails from './WorkDetails';
import { useGeneralSettingsStore } from '../../stores/useGeneralSettingsStore';
import { useWorkspaceModeStore } from '../../stores/useWorkspaceModeStore';

vi.mock('../../components/MarkdownRenderer', () => ({ default: ({ content }: { content: string }) => <p>{content}</p> }));
vi.mock('../../components/ToolDiffView', () => ({ default: () => null, shouldUseDiffView: () => false }));

beforeEach(() => {
  useWorkspaceModeStore.setState({ mode: 'code' });
  useGeneralSettingsStore.setState({ workDetailsByMode: { chat: 'standard', work: 'standard', study: 'standard', code: 'detailed' }, showCodingView: true });
});

const command = (i: number, extra: object = {}) => ({
  id: `c${i}`, name: 'Bash', status: 'done', startedAt: 1000 + i * 10, endedAt: 1005 + i * 10,
  input: { command: `echo ${i}`, description: `Step ${i}` }, result: { stdout: `out ${i}`, exit_code: 0 }, ...extra,
});
const steps = (...kinds: Array<string | { tool: string }>) => kinds.map((kind, i) => typeof kind === 'string'
  ? { id: `s${i}`, kind: kind === 'think' ? 'thinking' : 'text', text: kind === 'think' ? 'Inspect repositories' : kind }
  : { id: `tool:${kind.tool}`, kind: 'tool', toolId: kind.tool });

describe('collapsible tool-call groups', () => {
  it('shows ten consecutive commands as one collapsed group with every call inside', () => {
    const toolCalls = Array.from({ length: 10 }, (_, i) => command(i));
    const message = { role: 'assistant', content: 'Done.', toolCalls, processSteps: [
      ...toolCalls.map(tool => ({ id: `tool:${tool.id}`, kind: 'tool', toolId: tool.id })),
      { id: 'answer', kind: 'text', text: 'Done.' },
    ] };
    render(<WorkDetails message={message} index={0} live={false} />);
    const groups = screen.getAllByRole('button', { name: /^Ran commands/ });
    expect(groups).toHaveLength(1);
    expect(groups[0]).toHaveTextContent('Ran commands· 10');
    expect(groups[0]).toHaveAttribute('aria-expanded', 'false');
    fireEvent.click(groups[0]);
    const rows = screen.getAllByRole('button', { name: /Bash · Step \d/ });
    expect(rows.map(row => row.textContent?.match(/Step \d/)?.[0])).toEqual(toolCalls.map((_, i) => `Step ${i}`));
    expect(rows[9]).toHaveTextContent('└─');
    expect(rows[0]).toHaveTextContent('├─');
    fireEvent.click(rows[9]);
    expect(screen.getByText('out 9')).toBeInTheDocument();
  });

  it('starts a new group after visible text instead of appending to the previous one', () => {
    const toolCalls = [command(1), command(2), command(3)];
    const message = { role: 'assistant', content: 'Checked.Fixed.', toolCalls, processSteps: [
      { id: 'tool:c1', kind: 'tool', toolId: 'c1' }, { id: 'tool:c2', kind: 'tool', toolId: 'c2' },
      { id: 'mid', kind: 'text', text: 'Checked.' },
      { id: 'tool:c3', kind: 'tool', toolId: 'c3' }, { id: 'end', kind: 'text', text: 'Fixed.' },
    ] };
    const { container } = render(<WorkDetails message={message} index={0} live={false} />);
    const groups = screen.getAllByRole('button', { name: /^Ran commands/ });
    expect(groups.map(group => group.textContent)).toEqual(['Ran commands· 2', 'Ran commands']);
    const text = container.textContent || '';
    expect(text.indexOf('Ran commands· 2')).toBeLessThan(text.indexOf('Checked.'));
    expect(text.indexOf('Checked.')).toBeLessThan(text.lastIndexOf('Ran commands'));
  });

  it('keeps reasoning between calls inside the group in its original order', () => {
    const message = { role: 'assistant', content: '', toolCalls: [command(1), command(2)] };
    const items = groupWorkSteps(message, steps('think', { tool: 'c1' }, 'think', { tool: 'c2' }) as any, false);
    expect(items).toHaveLength(1);
    const group = items[0].kind === 'group' ? items[0].group : null;
    expect(group?.items.map(item => item.tool ? item.tool.id : 'think')).toEqual(['think', 'c1', 'think', 'c2']);
    expect(group).toMatchObject({ id: 'group:s0', status: 'completed', isExpanded: false, startTime: 1010, endTime: 1025 });
    render(<WorkDetails message={{ ...message, processSteps: steps('think', { tool: 'c1' }, 'think', { tool: 'c2' }) }} index={0} live={false} />);
    fireEvent.click(screen.getByRole('button', { name: /^Ran commands/ }));
    expect(screen.getAllByRole('button', { name: /Think · Inspect repositories/ })).toHaveLength(2);
  });

  it.each([
    [['Grep', 'Glob'], 'Searched files'], [['Read', 'Read'], 'Read files'], [['Edit', 'Write'], 'Edited files'],
    [['Bash', 'pwsh'], 'Ran commands'], [['Bash', 'Read'], 'Used tools'], [['WebFetch'], 'Used tools'],
  ])('names a group of %j "%s"', (names, title) => {
    expect(groupTitle(names as string[])).toBe(title);
  });

  it('opens while running, updates live, and collapses when finished unless the user chose', () => {
    const running = { role: 'assistant', content: '', toolCalls: [command(1), command(2, { status: 'running', result: undefined })],
      processSteps: [{ id: 'tool:c1', kind: 'tool', toolId: 'c1' }, { id: 'tool:c2', kind: 'tool', toolId: 'c2' }] };
    const { rerender } = render(<WorkDetails message={running} index={0} live />);
    const group = () => screen.getByRole('button', { name: /^Ran commands/ });
    expect(group()).toHaveAttribute('aria-expanded', 'true');
    expect(group()).toHaveTextContent('运行中');
    expect(within(screen.getByRole('button', { name: /Bash · Step 2/ })).getByRole('img', { name: '运行中' })).toBeInTheDocument();

    // A third call streams in: the same group grows instead of being recreated.
    const grown = { ...running, toolCalls: [...running.toolCalls, command(3, { status: 'running', result: undefined })],
      processSteps: [...running.processSteps, { id: 'tool:c3', kind: 'tool', toolId: 'c3' }] };
    rerender(<WorkDetails message={grown} index={0} live />);
    expect(screen.getAllByRole('button', { name: /^Ran commands/ })).toHaveLength(1);
    expect(screen.getAllByRole('button', { name: /Bash · Step/ })).toHaveLength(3);

    const done = { ...grown, content: 'All done.', toolCalls: grown.toolCalls.map(tool => ({ ...tool, status: 'done', result: { exit_code: 0 } })),
      processSteps: [...grown.processSteps, { id: 'answer', kind: 'text', text: 'All done.' }] };
    rerender(<WorkDetails message={done} index={0} live={false} />);
    expect(group()).toHaveAttribute('aria-expanded', 'false');

    // A manual choice survives later updates.
    fireEvent.click(group());
    rerender(<WorkDetails message={{ ...done }} index={0} live={false} />);
    expect(group()).toHaveAttribute('aria-expanded', 'true');
  });

  it('marks failed and interrupted calls without hiding their errors', () => {
    const message = { role: 'assistant', content: '', toolCalls: [
      command(1, { status: 'error', result: { stderr: 'permission denied', exit_code: 1 } }), command(2, { status: 'running', result: undefined }),
    ] };
    render(<WorkDetails message={message} index={0} live={false} />);
    const group = screen.getByRole('button', { name: /^Ran commands/ });
    expect(group).toHaveClass('text-red-500');
    fireEvent.click(group);
    fireEvent.click(screen.getByRole('button', { name: /Bash · Step 1/ }));
    expect(screen.getByText('permission denied')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Bash · Step 2/ })).toHaveTextContent('已中断');
  });

  it('only changes presentation: the message and its steps are untouched', () => {
    const message = { role: 'assistant', content: 'x', toolCalls: [command(1)], processSteps: steps({ tool: 'c1' }, 'x') };
    const before = JSON.stringify(message);
    groupWorkSteps(message, messageSteps(message), false);
    render(<WorkDetails message={message} index={0} live={false} />);
    expect(JSON.stringify(message)).toBe(before);
  });
});
