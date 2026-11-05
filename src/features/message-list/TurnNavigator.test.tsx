import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import TurnNavigator, { conversationTurns } from './TurnNavigator';

let frames: FrameRequestCallback[];
beforeEach(() => {
  frames = [];
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => frames.push(callback));
  vi.stubGlobal('cancelAnimationFrame', vi.fn());
  vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
});
afterEach(() => vi.unstubAllGlobals());
const messages = [
  { id: 'first', role: 'user', content: '第一个问题' },
  { role: 'assistant', content: 'First answer' },
  { role: 'user', content: 'Hidden summary', is_summary: 1 },
  { id: 'second', role: 'user', content: [{ type: 'image_url', image_url: { url: 'data:image/png;base64,x' } }] },
  { role: 'assistant', content: 'Second answer' },
];

it('builds previews from real messages, ignores compact boundaries and preserves attachment turns', () => {
  expect(conversationTurns(messages)).toEqual([
    { id: 'first', index: 0, prompt: '第一个问题', response: 'First answer' },
    { id: 'second', index: 3, prompt: '附件消息', response: 'Second answer' },
  ]);
});

it('navigates actual anchors, follows scroll and supports keyboard previews without touching a draft', () => {
  const scroller = document.createElement('div');
  scroller.innerHTML = '<div><div data-message-index="0"></div><div data-message-index="3"></div></div>';
  Object.defineProperties(scroller, { scrollHeight: { value: 1200 }, clientHeight: { value: 400 } });
  const anchors = scroller.querySelectorAll<HTMLElement>('[data-message-index]');
  anchors[0].getBoundingClientRect = () => ({ top: -scroller.scrollTop } as DOMRect);
  anchors[1].getBoundingClientRect = () => ({ top: 600 - scroller.scrollTop } as DOMRect);
  scroller.scrollTo = vi.fn();
  const onNavigate = vi.fn();
  render(<><input aria-label="Draft" defaultValue="keep this draft" /><TurnNavigator messages={messages} scrollRef={{ current: scroller }} onNavigate={onNavigate} /></>);
  act(() => { frames.splice(0).forEach(frame => frame(0)); });
  const first = screen.getByRole('button', { name: '跳转到第 1 轮' });
  const second = screen.getByRole('button', { name: '跳转到第 2 轮' });
  expect(first).toHaveAttribute('aria-current', 'true');
  fireEvent.mouseEnter(second);
  expect(screen.getByRole('tooltip')).toHaveTextContent('Second answer');
  fireEvent.click(second);
  expect(onNavigate).toHaveBeenCalledOnce();
  expect(scroller.scrollTo).toHaveBeenCalledWith({ top: 584, behavior: 'instant' });
  act(() => first.focus());
  fireEvent.keyDown(first, { key: 'End' });
  expect(second).toHaveFocus();
  fireEvent.keyDown(second, { key: 'Escape' });
  expect(screen.queryByRole('tooltip')).toBeNull();
  expect(screen.getByLabelText('Draft')).toHaveValue('keep this draft');
  scroller.scrollTop = 610;
  fireEvent.scroll(scroller);
  act(() => { frames.splice(0).forEach(frame => frame(0)); });
  expect(second).toHaveAttribute('aria-current', 'true');
});

it('hides the rail for a single turn and updates its preview during streaming', () => {
  const view = render(<TurnNavigator messages={messages.slice(0, 2)} scrollRef={{ current: null }} onNavigate={vi.fn()} />);
  expect(screen.queryByRole('navigation')).toBeNull();
  view.rerender(<TurnNavigator messages={messages} scrollRef={{ current: null }} onNavigate={vi.fn()} />);
  fireEvent.mouseEnter(screen.getByRole('button', { name: '跳转到第 2 轮' }));
  view.rerender(<TurnNavigator messages={[...messages.slice(0, 4), { role: 'assistant', content: 'Streaming answer grows' }]} scrollRef={{ current: null }} onNavigate={vi.fn()} />);
  expect(screen.getByRole('tooltip')).toHaveTextContent('Streaming answer grows');
});
