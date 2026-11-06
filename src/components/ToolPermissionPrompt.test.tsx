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
  it('keeps the prompt with an error after a rejected response', async () => {
    render(<ToolPermissionPrompt request={request} onRespond={vi.fn().mockRejectedValue(new Error('Request expired'))} />);
    fireEvent.click(screen.getByRole('button', { name: 'Allow' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('Request expired');
    expect(screen.getByRole('dialog')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Allow' })).toBeEnabled();
  });
});
