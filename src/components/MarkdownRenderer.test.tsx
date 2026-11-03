import React from 'react';
import { fireEvent, render, screen } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import MarkdownRenderer, { isUnclosedFence } from './MarkdownRenderer';

vi.mock('react-syntax-highlighter', () => ({ Prism: ({ children }: any) => <pre data-testid="highlight">{children}</pre> }));

describe('Markdown streaming blocks', () => {
  it('renders an unfinished fence once and preserves expansion as it grows and closes', () => {
    const code = Array.from({ length: 25 }, (_, i) => `line ${i}`).join('\n');
    const { rerender } = render(<MarkdownRenderer content={'```ts\n' + code} isStreaming />);
    expect(screen.getAllByTestId('highlight')).toHaveLength(1);
    expect(screen.getByText('generating')).toBeInTheDocument();
    fireEvent.click(screen.getByText('Show all'));
    rerender(<MarkdownRenderer content={'```ts\n' + code + '\nlast'} isStreaming />);
    expect(screen.getByText('Collapse')).toBeInTheDocument();
    expect(screen.getByTestId('highlight')).toHaveTextContent('last');
    rerender(<MarkdownRenderer content={'```ts\n' + code + '\nlast\n```\nMore'} isStreaming />);
    expect(screen.getAllByTestId('highlight')).toHaveLength(1);
    expect(screen.getByText('Collapse')).toBeInTheDocument();
    expect(screen.queryByText('generating')).not.toBeInTheDocument();
  });

  it('marks only the open fence and handles tildes, empty and unlabelled code', () => {
    render(<MarkdownRenderer content={'```\nclosed\n```\n\n~~~\n'} isStreaming />);
    expect(screen.getAllByTestId('highlight')).toHaveLength(2);
    expect(screen.getAllByText('generating')).toHaveLength(1);
  });

  it('keeps inline code inline and indented code in a block', () => {
    const { container } = render(<MarkdownRenderer content={'Use `x` here.\n\n    indented'} />);
    expect(container.querySelector('code.inline-code')).toHaveTextContent('x');
    expect(screen.getAllByTestId('highlight')).toHaveLength(1);
  });

  it('preserves math-like text inside unfinished tilde fences', () => {
    render(<MarkdownRenderer content={'~~~text\n$$first\nsecond$$'} isStreaming />);
    expect(screen.getByTestId('highlight').textContent).toBe('$$first\nsecond$$');
  });

  it('does not treat shorter fences, embedded markers or another marker as closing', () => {
    expect(isUnclosedFence('````ts\nx```\n```')).toBe(true);
    expect(isUnclosedFence('~~~\nx\n```')).toBe(true);
    expect(isUnclosedFence('````ts\nx\n`````')).toBe(false);
  });
});
