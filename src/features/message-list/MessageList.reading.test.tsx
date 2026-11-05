import React from 'react';
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/react';
import MessageList from './MessageList';

const props = {
  loading: false, expandedMessages: new Set<number>(), editingMessageIdx: null, editingContent: '',
  compactStatus: { state: '' }, onToggleExpand: vi.fn(), messageContentRefs: { current: new Map<number, HTMLDivElement>() }, t: (key: string) => key,
};
const messages = [
  { id: 'system', role: 'system', content: 'Internal instructions' },
  { id: 'user', role: 'user', content: 'Read this conversation' },
  { id: 'tools-only', role: 'assistant', content: '', thinking: 'Private reasoning', toolCalls: [{ id: 'x', name: 'Bash', input: { command: 'echo internal' }, result: 'internal output', status: 'done' }] },
  { id: 'answer', role: 'assistant', content: 'Before.\n\n## Answer\n\n**Result** with `code`.', thinking: 'Private reasoning',
    toolCalls: [{ id: 'y', name: 'Read', input: { file_path: 'notes.txt' }, result: 'tool output', status: 'done', textBefore: 'Before.\n\n' }],
    turn: { turnId: 'turn', status: 'completed', elapsedMs: 1000, startedAt: '2026-11-05T00:00:00Z', endedAt: '2026-11-05T00:00:01Z' },
  },
  { id: 'summary', role: 'assistant', content: 'Internal compaction', is_summary: 1 },
];

describe('reading mode', () => {
  it('keeps ordered user and assistant content, hides execution records without mutating them, and restores Chat', () => {
    const before = JSON.stringify(messages);
    const { container, rerender } = render(<MessageList {...props} messages={messages} readingMode />);
    expect([...container.querySelectorAll('[data-message-index]')].map(el => el.getAttribute('data-message-index'))).toEqual(['1', '3']);
    expect(screen.getByText('Read this conversation')).toBeVisible();
    expect(screen.getByText('Before.')).toBeVisible();
    expect(screen.getByRole('heading', { name: 'Answer' })).toBeVisible();
    expect(screen.getByText('Result').tagName).toBe('STRONG');
    expect(screen.getByText('code').tagName).toBe('CODE');
    expect(screen.getAllByRole('separator')).toHaveLength(1);
    expect(container.querySelector('[data-message-index="3"]')).toContainElement(screen.getByRole('separator'));
    expect(screen.queryByLabelText('Turn duration')).not.toBeInTheDocument();
    expect(container.textContent).not.toMatch(/Private reasoning|Internal instructions|internal output|tool output|Internal compaction|Worked for/);
    expect(screen.queryByRole('button', { name: /Read.*notes/ })).not.toBeInTheDocument();
    expect(JSON.stringify(messages)).toBe(before);
    rerender(<MessageList {...props} messages={messages} />);
    expect(container.querySelector('[data-message-index="2"]')).toBeInTheDocument();
    expect(screen.getByLabelText('Turn duration')).toBeVisible();
    expect(screen.queryByRole('separator')).not.toBeInTheDocument();
  });

  it('updates streamed text and preserves attachments and message indices', () => {
    const image = { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'aGVsbG8=' } };
    const initial = [{ role: 'user', content: [{ type: 'text', text: '图片问题' }, image] }, { role: 'assistant', content: '', thinking: 'Thinking' }];
    const { container, rerender } = render(<MessageList {...props} messages={initial} loading readingMode />);
    expect(container.querySelectorAll('[data-message-index]')).toHaveLength(1);
    fireEvent.click(screen.getByRole('button', { name: 'Open Image #1' }));
    expect(screen.getByRole('dialog')).toBeVisible();
    fireEvent.keyDown(document, { key: 'Escape' });
    for (const loading of [true, false]) {
      rerender(<MessageList {...props} messages={[initial[0], { ...initial[1], content: '真实回复 123' }]} loading={loading} readingMode />);
      expect(screen.getByText('真实回复 123')).toBeVisible();
      expect(container.querySelector('[data-message-index="1"]')).toBeInTheDocument();
      expect(screen.queryByText('Thinking')).not.toBeInTheDocument();
    }
  });
});
