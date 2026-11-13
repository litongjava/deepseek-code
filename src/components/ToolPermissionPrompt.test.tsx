import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import ToolPermissionPrompt from './ToolPermissionPrompt';

describe('tool permission prompt', () => {
  const request = { request_id: 'r', tool_use_id: 't', tool_name: 'Bash', input: { command: 'cd repo && npm test', description: 'Run tests' }, allow_session: true };
  it('shows a readable compound command and sends a session choice', async () => {
    const respond = vi.fn().mockResolvedValue(undefined);
    render(<ToolPermissionPrompt request={request} onRespond={respond} />);
    expect(screen.getByText(request.input.command)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Allow for Session' }));
    await waitFor(() => expect(respond).toHaveBeenCalledWith('allow_session'));
  });
  it.each([{ ...request, allow_session: false }, { ...request, tool_name: 'Write' }])('does not offer a session grant when unavailable', current => {
    render(<ToolPermissionPrompt request={current} onRespond={vi.fn()} />);
    expect(screen.queryByRole('button', { name: 'Allow for Session' })).toBeNull();
  });
  it('shows ToolScript provenance without changing the reviewed tool input or choices', async () => {
    const respond = vi.fn().mockResolvedValue(undefined);
    render(<ToolPermissionPrompt request={{ ...request, toolscript: { parent_tool_use_id: 'inv-parent', task_id: 'job' } }} onRespond={respond} />);
    expect(screen.getByText('From ToolScript (parent inv-parent)')).toBeInTheDocument();
    expect(screen.getByText(request.input.command)).toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: 'Allow' }));
    await waitFor(() => expect(respond).toHaveBeenCalledWith('allow'));
  });
  it('keeps the prompt with an error after a rejected response', async () => {
    render(<ToolPermissionPrompt request={request} onRespond={vi.fn().mockRejectedValue(new Error('Request expired'))} />);
    fireEvent.click(screen.getByRole('button', { name: 'Allow' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Request expired');
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Allow' })).toBeEnabled();
  });
  it('cancels the turn without granting or denying the tool while cancellation settles', async () => {
    const respond = vi.fn();
    let finish!: () => void;
    const cancel = vi.fn(() => new Promise<void>(resolve => { finish = resolve; }));
    render(<ToolPermissionPrompt request={request} onRespond={respond} onCancel={cancel} />);
    fireEvent.click(screen.getByRole('button', { name: 'Cancel turn' }));
    expect(cancel).toHaveBeenCalledOnce();
    expect(respond).not.toHaveBeenCalled();
    expect(screen.getByRole('button', { name: 'Allow' })).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Deny' })).toBeDisabled();
    finish();
    await waitFor(() => expect(screen.getByRole('button', { name: 'Cancel turn' })).toBeEnabled());
  });
  it('retains the pending prompt if turn cancellation fails', async () => {
    render(<ToolPermissionPrompt request={request} onRespond={vi.fn()} onCancel={vi.fn().mockRejectedValue(new Error('Stop failed'))} />);
    fireEvent.click(screen.getByRole('button', { name: 'Cancel turn' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Stop failed');
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Allow' })).toBeEnabled();
  });
});
