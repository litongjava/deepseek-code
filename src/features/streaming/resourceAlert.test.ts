import { describe, it, expect, vi } from 'vitest';
import { createStreamCallbacks } from './createStreamCallbacks';
import { nextResourceAlertToast } from './resourceAlert';

vi.mock('../../api', () => ({ getConversation: vi.fn(), getContextSize: vi.fn(), getAttachmentUrl: vi.fn(), compactConversation: vi.fn() }));
vi.mock('../../api/conversations', () => ({ persistMessages: vi.fn() }));
vi.mock('../../pyodideRunner', () => ({ executeCode: vi.fn(), sendCodeResult: vi.fn() }));
vi.mock('../../hooks/useAnalytics', () => ({ trackTokensUsed: vi.fn() }));

function harness(active = true) {
  let messages: any[] = [
    { id: 'user', role: 'user', content: 'check resources' },
    { id: 'assistant', role: 'assistant', content: '' },
  ];
  const onResourceAlert = vi.fn();
  const setMessagesFor = vi.fn((_id: string, update: (prev: any[]) => any[]) => { messages = update(messages); });
  const setMessages = vi.fn((update: (prev: any[]) => any[]) => { messages = update(messages); });
  const ctx: any = {
    conversationId: 'test', streamRequestId: 1, isStreamSessionActive: () => active,
    setMessagesFor, setMessages,
    removeStreaming: vi.fn(), clearStreamSession: vi.fn(), messagesBufferRef: { current: new Map() },
    activeRequestCountRef: { current: 1 }, viewingIdRef: { current: 'test' }, abortControllerRef: { current: null },
    setLoading: vi.fn(), setCompactStatus: vi.fn(), setContextInfo: vi.fn(), setTokenUsage: vi.fn(), setActiveTasks: vi.fn(),
    setAskUserDialog: vi.fn(), setToolPermissionDialog: vi.fn(), setPermissionApproval: vi.fn(), setPlanMode: vi.fn(), setConversationTitle: vi.fn(),
    onResourceAlert,
  };
  const updates = () => setMessagesFor.mock.calls.length + setMessages.mock.calls.length;
  return { callbacks: createStreamCallbacks(ctx), context: ctx, onResourceAlert, updates, message: () => messages[messages.length - 1] };
}

describe('resource alerts', () => {
  it.each(['onSystemEvents', 'onSystemEventsLight'] as const)('%s reports an active warning without touching the stream', callback => {
    const { callbacks, onResourceAlert, context, updates, message } = harness();
    callbacks.onDelta('Working...', 'Working...');
    callbacks.onToolUse({ type: 'start', tool_use_id: 'bash', tool_name: 'Bash', tool_input: { command: 'npm test' } });
    const before = JSON.parse(JSON.stringify(message()));
    const updatesBefore = updates();

    callbacks[callback]('resource_alert', 'Low memory: 1.0 GiB available', {
      type: 'resource_alert',
      kind: 'memory',
      summary: 'Low memory: 1.0 GiB available',
      details: 'Reduce parallel jobs and command output.',
    });

    expect(onResourceAlert).toHaveBeenCalledTimes(1);
    expect(onResourceAlert).toHaveBeenCalledWith({
      kind: 'memory',
      summary: 'Low memory: 1.0 GiB available',
      details: 'Reduce parallel jobs and command output.',
    });
    expect(JSON.parse(JSON.stringify(message()))).toEqual(before);
    expect(updates()).toBe(updatesBefore);
    expect(context.setLoading).not.toHaveBeenCalled();
    expect(message().content).toBe('Working...');
    expect(message().toolCalls[0]).toMatchObject({ id: 'bash', status: 'running' });
  });

  it('falls back to the forwarded message when the payload omits the summary', () => {
    const { callbacks, onResourceAlert } = harness();
    callbacks.onSystemEvents('resource_alert', 'Low CPU', { type: 'resource_alert', kind: 'cpu' });
    expect(onResourceAlert).toHaveBeenCalledWith({ kind: 'cpu', summary: 'Low CPU', details: undefined });
  });

  it.each(['onSystemEvents', 'onSystemEventsLight'] as const)('%s ignores alerts without a summary', callback => {
    const { callbacks, onResourceAlert, updates } = harness();
    const updatesBefore = updates();
    callbacks[callback]('resource_alert', '', { type: 'resource_alert', kind: 'memory' });
    callbacks[callback]('resource_alert', '   ', { type: 'resource_alert', kind: 'memory', summary: '   ' });
    expect(onResourceAlert).not.toHaveBeenCalled();
    expect(updates()).toBe(updatesBefore);
  });

  it.each(['onSystemEvents', 'onSystemEventsLight'] as const)('%s ignores alerts from an inactive stream session', callback => {
    const { callbacks, onResourceAlert } = harness(false);
    callbacks[callback]('resource_alert', 'Low disk', { type: 'resource_alert', kind: 'disk', summary: 'Low disk' });
    expect(onResourceAlert).not.toHaveBeenCalled();
  });
});

describe('resource alert toast', () => {
  it('prefixes and trims the summary', () => {
    expect(nextResourceAlertToast(null, '  Low memory: 1 GiB free  ')).toEqual({
      id: 1,
      text: 'System resource warning: Low memory: 1 GiB free',
    });
  });

  it('advances the id when the same summary arrives again so the timer restarts', () => {
    const first = nextResourceAlertToast(null, 'High CPU usage: 99.0% sustained');
    const second = nextResourceAlertToast(first, 'High CPU usage: 99.0% sustained');
    expect(second?.text).toBe(first?.text);
    expect(second?.id).toBe((first?.id ?? 0) + 1);
  });

  it('keeps the current toast for a blank or missing summary', () => {
    expect(nextResourceAlertToast(null, undefined)).toBeNull();
    expect(nextResourceAlertToast(null, '   ')).toBeNull();
    const current = { id: 4, text: 'System resource warning: Low disk' };
    expect(nextResourceAlertToast(current, '')).toBe(current);
  });
});
