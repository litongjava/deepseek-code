import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useComposerDrop } from './useComposerDrop';

function Harness({ onFiles, session = 'one' }: { onFiles: (files: FileList) => void; session?: string }) {
  const { composerRef, isDragging, handleDragOver, handleDragLeave, handleDrop } = useComposerDrop(onFiles, session);
  return <div ref={composerRef} data-testid="composer" data-dragging={isDragging} onDragOver={handleDragOver} onDragLeave={handleDragLeave} onDrop={handleDrop}><textarea aria-label="Prompt" /></div>;
}

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

describe('composer file drop', () => {
  it('ignores text drags and keeps file feedback when moving over children', () => {
    render(<Harness onFiles={vi.fn()} />);
    const composer = screen.getByTestId('composer');
    fireEvent.dragOver(composer, { dataTransfer: { types: ['text/plain'] } });
    expect(composer).toHaveAttribute('data-dragging', 'false');
    fireEvent.dragOver(composer, { dataTransfer: { types: ['Files'] } });
    expect(composer).toHaveAttribute('data-dragging', 'true');
    const leave = new Event('dragleave', { bubbles: true, cancelable: true });
    Object.defineProperty(leave, 'relatedTarget', { value: screen.getByRole('textbox') });
    act(() => composer.dispatchEvent(leave));
    expect(composer).toHaveAttribute('data-dragging', 'true');
    fireEvent.dragLeave(composer);
    expect(composer).toHaveAttribute('data-dragging', 'false');
  });

  it('delivers files once and resets feedback on drop and session switch', () => {
    const onFiles = vi.fn();
    const view = render(<Harness onFiles={onFiles} />);
    const composer = screen.getByTestId('composer');
    const files = [new File(['hello'], 'hello.txt')];
    fireEvent.dragOver(composer, { dataTransfer: { types: ['Files'] } });
    fireEvent.drop(composer, { dataTransfer: { files } });
    expect(onFiles).toHaveBeenCalledExactlyOnceWith(files);
    expect(composer).toHaveAttribute('data-dragging', 'false');
    fireEvent.dragOver(composer, { dataTransfer: { types: ['Files'] } });
    view.rerender(<Harness onFiles={onFiles} session="two" />);
    expect(composer).toHaveAttribute('data-dragging', 'false');
  });

  it('uses a monotonic spring and cancels it when leaving', () => {
    const cancel = vi.fn();
    const animate = vi.fn(() => ({ cancel }));
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: false })));
    const original = Element.prototype.animate;
    Element.prototype.animate = animate as any;
    try {
      render(<Harness onFiles={vi.fn()} />);
      const composer = screen.getByTestId('composer');
      fireEvent.dragOver(composer, { dataTransfer: { types: ['Files'] } });
      const frames = (animate.mock.calls[0] as unknown as [Keyframe[]])[0];
      const scales = frames.map(frame => Number(String(frame.transform).slice(6, -1)));
      expect(scales[0]).toBe(0.996);
      expect(scales[scales.length - 1]).toBe(1);
      expect(scales.every((scale, index) => scale <= 1 && (index === 0 || scale >= scales[index - 1]))).toBe(true);
      fireEvent.dragLeave(composer);
      expect(cancel).toHaveBeenCalledOnce();
    } finally {
      Element.prototype.animate = original;
    }
  });
});
