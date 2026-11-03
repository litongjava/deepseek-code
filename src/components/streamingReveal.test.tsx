import React from 'react';
import { act, render } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import MarkdownRenderer from './MarkdownRenderer';

vi.mock('react-syntax-highlighter', () => ({ Prism: ({ children }: any) => <pre>{children}</pre> }));
afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

describe('bounded streaming reveal', () => {
  it('does not animate history, caps new spans, and compacts after 300ms', () => {
    vi.useFakeTimers();
    const { container, rerender } = render(<MarkdownRenderer content="History" isStreaming />);
    const spans = () => container.querySelectorAll('.streaming-reveal-grapheme');
    expect(spans()).toHaveLength(0);
    rerender(<MarkdownRenderer content={'History ' + '字'.repeat(1000)} isStreaming />);
    expect(spans()).toHaveLength(8);
    expect(container.textContent).toBe('History ' + '字'.repeat(1000));
    act(() => { vi.advanceTimersByTime(300); });
    expect(spans()).toHaveLength(0);
  });

  it('keeps emoji and combining clusters intact and clears on finish/replacement', () => {
    const { container, rerender } = render(<MarkdownRenderer content="Start " isStreaming />);
    rerender(<MarkdownRenderer content="Start 👨‍👩‍👧‍👦é🇨🇳" isStreaming />);
    expect(Array.from(container.querySelectorAll('.streaming-reveal-grapheme')).map(el => el.textContent)).toEqual(['👨‍👩‍👧‍👦', 'é', '🇨🇳']);
    rerender(<MarkdownRenderer content="Start 👨‍👩‍👧‍👦é🇨🇳" />);
    expect(container.querySelectorAll('.streaming-reveal-grapheme')).toHaveLength(0);
    rerender(<MarkdownRenderer content="Other conversation" isStreaming />);
    expect(container.querySelectorAll('.streaming-reveal-grapheme')).toHaveLength(0);
  });

  it('never wraps code, links, or math and does not accumulate fast batches', () => {
    const { container, rerender } = render(<MarkdownRenderer content="Start " isStreaming />);
    let text = 'Start `code` [link](https://example.com) $x^2$\n\n```ts\ncode\n```\n\nProse';
    rerender(<MarkdownRenderer content={text} isStreaming />);
    expect(container.querySelector('a .streaming-reveal-grapheme, code .streaming-reveal-grapheme, pre .streaming-reveal-grapheme, .katex .streaming-reveal-grapheme')).toBeNull();
    for (let i = 0; i < 20; i++) {
      text += 'abcdefghi';
      rerender(<MarkdownRenderer content={text} isStreaming />);
      expect(container.querySelectorAll('.streaming-reveal-grapheme').length).toBeLessThanOrEqual(8);
    }
  });

  it('lets an in-flight batch finish without restarting or replacing its spans on fast appends', () => {
    vi.useFakeTimers();
    const { container, rerender, unmount } = render(<MarkdownRenderer content="Start " isStreaming />);
    rerender(<MarkdownRenderer content="Start abc" isStreaming />);
    const original = Array.from(container.querySelectorAll<HTMLElement>('.streaming-reveal-grapheme'));
    expect(original.map(span => span.style.animationDelay)).toEqual(['0ms', '30ms', '60ms']);
    const animationName = original[0].style.animationName;
    act(() => { vi.advanceTimersByTime(20); });
    rerender(<MarkdownRenderer content="Start abcdef" isStreaming />);
    const current = Array.from(container.querySelectorAll<HTMLElement>('.streaming-reveal-grapheme'));
    expect(current.map(span => span.textContent)).toEqual(['a', 'b', 'c']);
    current.forEach((span, index) => expect(span).toBe(original[index]));
    expect(current.map(span => span.style.animationDelay)).toEqual(['0ms', '30ms', '60ms']);
    expect(container.textContent).toBe('Start abcdef');
    act(() => { vi.advanceTimersByTime(280); });
    expect(container.querySelectorAll('.streaming-reveal-grapheme')).toHaveLength(0);
    rerender(<MarkdownRenderer content="Start abcdefg" isStreaming />);
    const next = container.querySelector<HTMLElement>('.streaming-reveal-grapheme')!;
    expect(next.textContent).toBe('g');
    expect(next.style.animationName).not.toBe(animationName);
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('does not split a cluster when a combining mark arrives during a reveal', () => {
    const { container, rerender } = render(<MarkdownRenderer content="Start " isStreaming />);
    rerender(<MarkdownRenderer content="Start e" isStreaming />);
    rerender(<MarkdownRenderer content="Start é" isStreaming />);
    expect(container.textContent).toBe('Start é');
    expect(container.querySelectorAll('.streaming-reveal-grapheme')).toHaveLength(0);
  });

  it('immediately removes spans when reduced motion is enabled and cleans its listener', () => {
    let listener: () => void = () => {};
    const remove = vi.fn();
    const media = { matches: false, addEventListener: vi.fn((_type, callback) => { listener = callback; }), removeEventListener: remove };
    vi.spyOn(window, 'matchMedia').mockReturnValue(media as any);
    const { container, rerender, unmount } = render(<MarkdownRenderer content="Start " isStreaming />);
    rerender(<MarkdownRenderer content="Start text" isStreaming />);
    expect(container.querySelectorAll('.streaming-reveal-grapheme').length).toBeGreaterThan(0);
    act(() => { media.matches = true; listener(); });
    expect(container.querySelectorAll('.streaming-reveal-grapheme')).toHaveLength(0);
    rerender(<MarkdownRenderer content="Start text plus more" isStreaming />);
    expect(container.querySelectorAll('.streaming-reveal-grapheme')).toHaveLength(0);
    unmount();
    expect(remove).toHaveBeenCalledWith('change', listener);
  });
});
