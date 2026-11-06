import React from 'react';
import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import MessageList from './MessageList';

const baseProps = {
  expandedMessages: new Set<number>(), editingMessageIdx: null, editingContent: '',
  compactStatus: { state: 'idle' }, onToggleExpand: vi.fn(),
  messageContentRefs: { current: new Map<number, HTMLDivElement>() }, t: (key: string) => key,
};

describe('reply indicator', () => {
  it('shows the indicator while the running answer has no text yet', () => {
    render(<MessageList {...baseProps} loading messages={[{ role: 'user', content: 'Question' }, { role: 'assistant', content: '' }]} />);
    expect(screen.getByRole('status')).toHaveTextContent('Working on it...');
  });

  it('keeps the indicator while the model is thinking without visible output', () => {
    render(<MessageList {...baseProps} loading messages={[
      { role: 'user', content: 'Question' },
      { role: 'assistant', content: '', thinking: 'Reasoning in progress', isThinking: true },
    ]} />);
    expect(screen.getByRole('status')).toHaveTextContent('Working on it...');
  });

  it('renders the indicator for the running answer in reading mode', () => {
    const { container } = render(<MessageList {...baseProps} loading readingMode messages={[
      { role: 'user', content: 'Question' },
      { role: 'assistant', content: '', thinking: 'Reasoning in progress', isThinking: true },
    ]} />);
    expect(container.querySelector('[data-message-index="1"]')).toBeInTheDocument();
    expect(screen.getByRole('status')).toHaveTextContent('Working on it...');
    expect(screen.queryByText('Reasoning in progress')).not.toBeInTheDocument();
  });
});
