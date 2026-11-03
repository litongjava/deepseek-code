import { act, fireEvent, render } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useScrollToBottom } from './useScrollToBottom';

let api: ReturnType<typeof useScrollToBottom>;
let frames: Map<number, FrameRequestCallback>;
let nextFrame: number;
let resize: () => void;
const disconnect = vi.fn();
function Harness({ visible = true, session = 'one', text = 'hello' }) {
  api = useScrollToBottom([text], true, 50, session);
  return visible ? <div ref={api.scrollContainerRef} data-testid="scroll"><div>{text}</div></div> : null;
}
function flush() { act(() => { const pending = [...frames.values()]; frames.clear(); pending.forEach(callback => callback(0)); }); }
function geometry(element: HTMLElement) {
  Object.defineProperties(element, { scrollHeight: { configurable: true, value: 1000 }, clientHeight: { configurable: true, value: 200 } });
  element.scrollTop = 800;
  element.scrollTo = vi.fn();
}
beforeEach(() => {
  frames = new Map(); nextFrame = 0;
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => { frames.set(++nextFrame, callback); return nextFrame; });
  vi.stubGlobal('cancelAnimationFrame', (id: number) => frames.delete(id));
  vi.stubGlobal('ResizeObserver', class { constructor(callback: () => void) { resize = callback; } observe() {} disconnect = disconnect; });
});
afterEach(() => { vi.unstubAllGlobals(); vi.clearAllMocks(); });

describe('useScrollToBottom', () => {
  it('binds a late container, coalesces frames and cleans up', () => {
    const view = render(<Harness visible={false} />);
    expect(frames.size).toBe(0);
    view.rerender(<Harness />);
    const element = view.getByTestId('scroll'); geometry(element);
    act(() => { resize(); resize(); api.scheduleScrollToBottomAfterRender(); });
    expect(frames.size).toBe(1);
    flush();
    expect(element.scrollTo).toHaveBeenCalledTimes(1);
    act(() => resize());
    view.unmount();
    expect(frames.size).toBe(0);
    expect(disconnect).toHaveBeenCalled();
  });

  it('does not steal scroll after upward input and resumes at bottom', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    fireEvent.wheel(element, { deltaY: -20 });
    element.scrollTop = 400;
    fireEvent.scroll(element);
    act(() => resize());
    expect(frames.size).toBe(0);
    expect(api.isAtBottomRef.current).toBe(false);
    element.scrollTop = 800; fireEvent.scroll(element);
    act(() => resize()); flush();
    expect(api.isAtBottomRef.current).toBe(true);
    expect(element.scrollTo).toHaveBeenLastCalledWith({ top: 1000, behavior: 'auto' });
  });

  it('keeps a small upward gesture paused even inside the bottom threshold', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    fireEvent.wheel(element, { deltaY: -20 });
    fireEvent.scroll(element);
    expect(api.isAtBottomRef.current).toBe(false);
    element.scrollTop = 780;
    fireEvent.scroll(element);
    act(() => { api.handleScroll(); resize(); });
    expect(api.isAtBottomRef.current).toBe(false);
    expect(frames.size).toBe(0);
    element.scrollTop = 800;
    fireEvent.scroll(element);
    expect(api.isAtBottomRef.current).toBe(true);
  });

  it('detects upward scrollbar movement without a wheel event', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    element.scrollTop = 780;
    fireEvent.scroll(element);
    act(() => resize());
    expect(api.isAtBottomRef.current).toBe(false);
    expect(frames.size).toBe(0);
  });

  it('retains follow when shrinking content clamps the scroll position', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    Object.defineProperty(element, 'scrollHeight', { configurable: true, value: 500 });
    element.scrollTop = 300;
    fireEvent.scroll(element);
    act(() => resize());
    expect(api.isAtBottomRef.current).toBe(true);
    expect(frames.size).toBe(1);
    flush();
    expect(element.scrollTo).toHaveBeenLastCalledWith({ top: 500, behavior: 'auto' });
  });

  it('keeps following growth without treating a stale scroll event as upward intent', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    Object.defineProperty(element, 'scrollHeight', { configurable: true, value: 1600 });
    fireEvent.scroll(element);
    act(() => resize());
    expect(api.isAtBottomRef.current).toBe(true);
    expect(frames.size).toBe(1);
    flush();
    expect(element.scrollTo).toHaveBeenLastCalledWith({ top: 1600, behavior: 'auto' });
  });

  it('resets the position baseline when switching to a shorter session', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    fireEvent.wheel(element, { deltaY: -20 });
    Object.defineProperty(element, 'scrollHeight', { configurable: true, value: 500 });
    element.scrollTop = 300;
    view.rerender(<Harness session="two" />);
    fireEvent.scroll(element);
    expect(api.isAtBottomRef.current).toBe(true);
    expect(frames.size).toBe(1);
    flush();
    expect(element.scrollTo).toHaveBeenLastCalledWith({ top: 500, behavior: 'auto' });
  });

  it('cancels keyboard/touch follow and resets it on session switch', () => {
    const view = render(<Harness />); const element = view.getByTestId('scroll'); geometry(element); flush();
    fireEvent.keyDown(element, { key: 'PageUp' });
    expect(api.isAtBottomRef.current).toBe(false);
    view.rerender(<Harness session="two" />);
    expect(api.isAtBottomRef.current).toBe(true);
    expect(frames.size).toBe(1);
    fireEvent.touchStart(element, { touches: [{ clientY: 100 }] });
    fireEvent.touchMove(element, { touches: [{ clientY: 150 }] });
    expect(frames.size).toBe(0);
    expect(api.isAtBottomRef.current).toBe(false);
  });
});
