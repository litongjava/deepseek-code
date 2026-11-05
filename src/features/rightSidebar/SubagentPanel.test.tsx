import { cleanup, fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter, useLocation } from 'react-router-dom';
import { afterEach, expect, it, vi } from 'vitest';
import SubagentPanel from './SubagentPanel';
import { useSubagentTranscript } from './useSubagentTranscript';
vi.mock('./useSubagentTranscript', () => ({ useSubagentTranscript: vi.fn() }));
vi.mock('../message-list/MessageList', () => ({ default: (props: any) => <div data-testid="transcript">{props.conversationId}:{String(props.readOnly)}:{props.messages[0]?.content}</div> }));
const invocation = { parent_session_id: 'direct-parent', parent_message_id: 'm', tool_use_id: 't', child_session_id: 'child', attempt_id: 'a', mode: 'foreground', status: 'running', description: 'Child task' };
function Location() { return <span data-testid="route">{useLocation().pathname}</span>; }
afterEach(cleanup);
it('uses isolated child scope and navigates to its direct parent without retargeting the transcript', () => {
  vi.mocked(useSubagentTranscript).mockReturnValue({ snapshot: { invocation, messages: [{ id: 'a', content: 'full history' }], next_offset: null, cursor: 'x', status: 'completed', parent_exists: true }, loading: false, error: '', retry: vi.fn() });
  render(<MemoryRouter initialEntries={['/chat/unrelated']}><Location /><SubagentPanel invocation={invocation} active /></MemoryRouter>);
  expect(screen.getByTestId('transcript').textContent).toBe('child:true:full history');
  fireEvent.click(screen.getByRole('button', { name: 'Go to parent conversation' }));
  expect(screen.getByTestId('route').textContent).toBe('/chat/direct-parent');
  expect(screen.getByTestId('transcript').textContent).toBe('child:true:full history');
});
it('distinguishes deleted child and missing parent without enabling navigation', () => {
  vi.mocked(useSubagentTranscript).mockReturnValue({ snapshot: { invocation, messages: [], next_offset: null, cursor: 'x', status: 'missing', parent_exists: false }, loading: false, error: '', retry: vi.fn() });
  render(<MemoryRouter><SubagentPanel invocation={invocation} active={false} /></MemoryRouter>);
  expect(screen.getByText('Subagent conversation missing or deleted.')).toBeTruthy();
  expect(screen.getByRole('button', { name: 'Go to parent conversation' })).toBeDisabled();
  expect(useSubagentTranscript).toHaveBeenLastCalledWith('direct-parent', 'child', false);
});
