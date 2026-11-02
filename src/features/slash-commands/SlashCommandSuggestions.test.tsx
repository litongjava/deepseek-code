import { render, screen, fireEvent } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import SlashCommandSuggestions from './SlashCommandSuggestions';

const commands = Array.from({ length: 20 }, (_, index) => ({
  name: `/command${index}`,
  description: `Command ${index}`,
}));

function fixture(top: number, bottom: number) {
  const anchor = document.createElement('div');
  const boundary = document.createElement('div');
  anchor.getBoundingClientRect = () => ({ top, bottom, left: 20, width: 500 }) as DOMRect;
  boundary.getBoundingClientRect = () => ({ top: 0, bottom: 800 }) as DOMRect;
  return { anchorRef: { current: anchor }, boundaryRef: { current: boundary }, onSelect: vi.fn() };
}

afterEach(() => vi.restoreAllMocks());

describe('SlashCommandSuggestions positioning', () => {
  it('keeps the menu below the composer while filtering and restoring results', () => {
    vi.spyOn(window, 'innerHeight', 'get').mockReturnValue(800);
    // A short result fits above, but the full menu only fits below.
    vi.spyOn(HTMLElement.prototype, 'scrollHeight', 'get').mockImplementation(function (this: HTMLElement) {
      return this.children.length * 40;
    });
    const props = fixture(220, 320);
    const { rerender } = render(<SlashCommandSuggestions {...props} commands={commands} />);
    const menu = screen.getByLabelText('Slash commands');
    expect(menu.style.top).toBe('324px');
    rerender(<SlashCommandSuggestions {...props} commands={commands.slice(0, 1)} />);
    fireEvent.resize(window);
    expect(menu.style.top).toBe('324px');
    expect(menu.style.bottom).toBe('');
    rerender(<SlashCommandSuggestions {...props} commands={commands} />);
    expect(menu.style.top).toBe('324px');
  });

  it('anchors the bottom edge above the composer and still selects filtered commands', () => {
    vi.spyOn(window, 'innerHeight', 'get').mockReturnValue(800);
    const props = fixture(600, 700);
    const { rerender } = render(<SlashCommandSuggestions {...props} commands={commands} />);
    const menu = screen.getByLabelText('Slash commands');
    expect(menu.style.bottom).toBe('204px');
    expect(menu.style.top).toBe('');
    rerender(<SlashCommandSuggestions {...props} commands={commands.slice(0, 1)} />);
    expect(menu.style.bottom).toBe('204px');
    fireEvent.click(screen.getByRole('button'));
    expect(props.onSelect).toHaveBeenCalledWith('/command0');
    props.anchorRef.current.getBoundingClientRect = () => ({ top: 20, bottom: 120, left: 20, width: 500 }) as DOMRect;
    fireEvent.resize(window);
    expect(menu.style.top).toBe('124px');
    expect(menu.style.bottom).toBe('');
  });
});
