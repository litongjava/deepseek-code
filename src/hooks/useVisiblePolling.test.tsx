import { act, render } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { useVisiblePolling } from './useVisiblePolling';

function Poller({ poll, ms = 1000 }: { poll: () => unknown; ms?: number }) {
  useVisiblePolling(poll, ms, []);
  return null;
}

afterEach(() => { vi.useRealTimers(); delete (document as { hidden?: boolean }).hidden; });

describe('useVisiblePolling', () => {
  it('waits for a slow call before scheduling the next one', async () => {
    vi.useFakeTimers();
    let finish!: () => void;
    const poll = vi.fn(() => new Promise<void>(resolve => { finish = resolve; }));
    render(<Poller poll={poll} />);
    expect(poll).toHaveBeenCalledTimes(1);
    await act(async () => { vi.advanceTimersByTime(5000); });
    expect(poll).toHaveBeenCalledTimes(1);
    await act(async () => { finish(); });
    await act(async () => { vi.advanceTimersByTime(1000); });
    expect(poll).toHaveBeenCalledTimes(2);
  });

  it('pauses in a hidden page and refreshes when it is shown', async () => {
    vi.useFakeTimers();
    const poll = vi.fn();
    render(<Poller poll={poll} />);
    await act(async () => {});
    Object.defineProperty(document, 'hidden', { configurable: true, value: true });
    await act(async () => { vi.advanceTimersByTime(1000); });
    const calls = poll.mock.calls.length;
    await act(async () => { vi.advanceTimersByTime(10000); });
    expect(poll).toHaveBeenCalledTimes(calls);
    Object.defineProperty(document, 'hidden', { configurable: true, value: false });
    await act(async () => { document.dispatchEvent(new Event('visibilitychange')); });
    expect(poll).toHaveBeenCalledTimes(calls + 1);
  });

  it('stops after unmount', async () => {
    vi.useFakeTimers();
    const poll = vi.fn();
    const view = render(<Poller poll={poll} />);
    await act(async () => {});
    view.unmount();
    await act(async () => { vi.advanceTimersByTime(10000); });
    expect(poll).toHaveBeenCalledTimes(1);
  });
});
