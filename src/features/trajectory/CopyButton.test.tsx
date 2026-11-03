import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import CopyButton from './CopyButton';
import { copyToClipboard } from '../../utils/clipboard';
vi.mock('../../utils/clipboard', () => ({ copyToClipboard: vi.fn() }));

afterEach(() => { vi.useRealTimers(); vi.restoreAllMocks(); });

describe('CopyButton feedback', () => {
  it('clears success and cleans its timeout on unmount', async () => {
    vi.useFakeTimers();
    vi.mocked(copyToClipboard).mockResolvedValue(true);
    const view = render(<CopyButton text="sample" />);
    await act(async () => { fireEvent.click(screen.getByRole('button')); });
    expect(screen.getByRole('button')).toHaveAttribute('title', '已复制');
    act(() => vi.advanceTimersByTime(1800));
    expect(screen.getByRole('button')).toHaveAttribute('title', '复制');
    await act(async () => { fireEvent.click(screen.getByRole('button')); });
    view.unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it('skips confirmation motion when reduced motion is requested', async () => {
    vi.stubGlobal('matchMedia', vi.fn(() => ({ matches: true })));
    const animate = vi.fn();
    const original = SVGElement.prototype.animate;
    SVGElement.prototype.animate = animate;
    try {
      vi.mocked(copyToClipboard).mockResolvedValue(true);
      render(<CopyButton text="sample" />);
      await act(async () => { fireEvent.click(screen.getByRole('button')); });
      expect(animate).not.toHaveBeenCalled();
      expect(screen.getByRole('button')).toHaveAttribute('title', '已复制');
    } finally {
      SVGElement.prototype.animate = original;
      vi.unstubAllGlobals();
    }
  });

  it('restarts the success lifetime after repeated copies', async () => {
    vi.useFakeTimers();
    vi.mocked(copyToClipboard).mockResolvedValue(true);
    render(<CopyButton text="sample" />);
    await act(async () => { fireEvent.click(screen.getByRole('button')); });
    act(() => vi.advanceTimersByTime(1500));
    await act(async () => { fireEvent.click(screen.getByRole('button')); });
    act(() => vi.advanceTimersByTime(500));
    expect(screen.getByRole('button')).toHaveAttribute('title', '已复制');
    act(() => vi.advanceTimersByTime(1300));
    expect(screen.getByRole('button')).toHaveAttribute('title', '复制');
  });

  it('ignores stale clipboard results and results after unmount', async () => {
    vi.useFakeTimers();
    let finish!: (success: boolean) => void;
    vi.mocked(copyToClipboard).mockImplementationOnce(() => new Promise(resolve => { finish = resolve; })).mockResolvedValue(false);
    const view = render(<CopyButton text="sample" />);
    fireEvent.click(screen.getByRole('button'));
    await act(async () => { fireEvent.click(screen.getByRole('button')); });
    await act(async () => finish(true));
    expect(screen.getByRole('button')).toHaveAttribute('title', '复制失败');
    vi.mocked(copyToClipboard).mockImplementationOnce(() => new Promise(resolve => { finish = resolve; }));
    fireEvent.click(screen.getByRole('button'));
    view.unmount();
    await act(async () => finish(true));
    expect(vi.getTimerCount()).toBe(0);
  });

  it('keeps errors visible instead of expiring them', async () => {
    vi.useFakeTimers();
    vi.mocked(copyToClipboard).mockResolvedValue(false);
    render(<CopyButton text="sample" />);
    await act(async () => { fireEvent.click(screen.getByRole('button')); });
    act(() => vi.advanceTimersByTime(5000));
    expect(screen.getByRole('button')).toHaveAttribute('title', '复制失败');
  });
});
