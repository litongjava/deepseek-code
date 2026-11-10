import React from 'react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import MessageList from './MessageList';
import { applyTurnTiming, turnLabel } from './turnTiming';
import { restoreProcessMetadata } from '../trajectory/model';

vi.mock('../trajectory/WorkDetails', () => ({ default: () => <div>Answer</div> }));
vi.mock('./ResponseFooter', () => ({ responseDividerClassName: 'h-px w-full bg-deepseek-border', default: () => <div>Response actions</div> }));
afterEach(cleanup);
const turn = { turnId: 'one', startedAt: '2026-11-03T00:00:00Z', endedAt: '2026-11-03T00:02:08Z', elapsedMs: 128999, status: 'completed' as const };
function list(messages: any[], loading = false) {
  return render(<MessageList messages={messages} loading={loading} expandedMessages={new Set()} editingMessageIdx={null} editingContent="" compactStatus={{ state: '' }} onSetEditingContent={vi.fn()} onEditCancel={vi.fn()} onEditSave={vi.fn()} onToggleExpand={vi.fn()} onResend={vi.fn()} onEdit={vi.fn()} onBranch={vi.fn()} onSetMessages={vi.fn()} messageContentRefs={{ current: new Map() }} t={key => key} />);
}

describe('message images', () => {
  const image = { type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'aGVsbG8=' } };
  it('renders mixed blocks in order with non-contiguous labels and clickable large images', () => {
    list([{ role: 'user', content: [{ type: 'text', text: '[Image #3] before' }, image, { type: 'text', text: '[Image #8] between' }, image] }]);
    const images = screen.getAllByRole('img');
    expect(images.map(element => element.getAttribute('alt'))).toEqual(['Image #3', 'Image #8']);
    expect(images[0].getAttribute('src')).toBe('data:image/png;base64,aGVsbG8=');
    fireEvent.click(screen.getByRole('button', { name: 'Open Image #8' }));
    expect(screen.getByRole('dialog')).toBeTruthy();
    fireEvent.keyDown(document, { key: 'Escape' });
    expect(screen.queryByRole('dialog')).toBeNull();
    fireEvent.error(images[0]);
    expect(screen.getByText(/Image #3 — Image unavailable/)).toBeTruthy();
  });
  it('explains historical placeholders and malformed saved data without fetching a path', () => {
    list([{ role: 'user', content: '[Image #9]' }, { role: 'user', content: [{ type: 'image_url', image_url: { url: 'file:///private.png' } }] }]);
    expect(screen.getAllByRole('note')).toHaveLength(2);
    expect(screen.queryByRole('img')).toBeNull();
  });
});

describe('durable turn timing', () => {
  it('does not render a zero for an empty cancelled turn', () => {
    list([{ role: 'assistant', content: '', thinking: '', toolCalls: [], turn: { ...turn, status: 'cancelled' } }]);
    expect(screen.getByText('Worked for 2m 08s · Cancelled')).toBeVisible();
    expect(screen.queryByText('0', { exact: true })).not.toBeInTheDocument();
    expect(screen.queryByText('Response actions')).not.toBeInTheDocument();
  });
  it.each([[0, '0s'], [59999, '59s'], [60000, '1m 00s'], [128999, '2m 08s'], [3903000, '65m 03s']])('formats %i milliseconds', (elapsedMs, label) => {
    expect(turnLabel({ ...turn, elapsedMs })).toBe(`Worked for ${label}`);
  });
  it('shows one heading for multiple segments, including empty failed turns', () => {
    list([{ role: 'assistant', content: 'first', turn }, { role: 'assistant', content: 'second', turn }, { role: 'assistant', content: '', turn: { ...turn, turnId: 'two', status: 'failed' } }]);
    expect(screen.getAllByLabelText('Turn duration')).toHaveLength(2);
    expect(screen.getByText('Worked for 2m 08s · Failed')).toBeTruthy();
    expect(screen.getAllByText('Response actions')).toHaveLength(2);
  });
  it('does not derive duration from message times, stale local cache or interrupted running history', () => {
    list([{ role: 'assistant', content: 'legacy', processStartedAt: 1, processEndedAt: 9999 }, { role: 'assistant', turn: { ...turn, status: 'running', elapsedMs: null, endedAt: null } }]);
    expect(screen.queryByLabelText('Turn duration')).toBeNull();
    expect(restoreProcessMetadata({ id: 'm', role: 'assistant', content: 'same' }, 0, [{ id: 'm', role: 'assistant', content: 'same', turn }]).turn).toBeUndefined();
  });
  it('updates every same-turn segment, preserves other turns and ignores malformed timing', () => {
    const messages = [{ role: 'assistant', turn: { ...turn, status: 'running' } }, { role: 'assistant', turn }, { role: 'assistant', turn: { ...turn, turnId: 'two' } }];
    expect(applyTurnTiming(messages, { ...turn, status: 'cancelled' }).map(m => m.turn.status)).toEqual(['cancelled', 'cancelled', 'completed']);
    expect(applyTurnTiming(messages, { ...turn, elapsedMs: -1 })).toBe(messages);
  });
});
