import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import PlanApprovalController from './PlanApprovalController';
import { useToolStore } from '../../stores/useToolStore';
import { useChatStore } from '../../stores/useChatStore';
import { request } from '../../api/client';

vi.mock('../../api/client', () => ({ request: vi.fn() }));
vi.mock('../../components/MarkdownRenderer', () => ({ default: ({ content }: { content: string }) => <article>{content}</article> }));
const plan = {
  type: 'plan_approval', version: 1, sessionId: 'session-a', requestId: 'review-a', toolUseId: 'tool-a',
  planVersion: 'version-a', plan: '# Actual reviewed plan', planFilePath: 'plans/a.md', status: 'pending',
  choices: [{ mode: 'ask', context: 'keep', enabled: true }],
};
const snapshot = () => ({ json: async () => ({ requests: [{ planApprovalRequest: plan }] }) }) as Response;

afterEach(() => { vi.resetAllMocks(); useToolStore.getState().setAskUserDialog(null); });
describe('PlanApprovalController', () => {
  it('recovers pending approval and keeps feedback when hidden and reopened', async () => {
    vi.mocked(request).mockResolvedValue(snapshot());
    render(<PlanApprovalController sessionId="session-a" />);
    await screen.findByRole('dialog', { name: '审阅执行计划' });
    fireEvent.click(screen.getByRole('button', { name: '提出修改' }));
    fireEvent.change(screen.getByRole('textbox'), { target: { value: 'Keep this feedback' } });
    fireEvent.click(screen.getByRole('button', { name: '收起计划审批' }));
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('button', { name: '待审阅计划' }));
    expect(screen.getByRole('textbox')).toHaveValue('Keep this feedback');
  });

  it('waits for applied confirmation and sends exact server identity', async () => {
    let complete!: (response: Response) => void;
    vi.mocked(request).mockImplementation(async (_path, options) => options?.method === 'POST'
      ? new Promise(resolve => { complete = resolve; }) : snapshot());
    render(<PlanApprovalController sessionId="session-a" />);
    fireEvent.click(await screen.findByRole('button', { name: '批准并开始' }));
    expect(screen.getByRole('button', { name: '提交中…' })).toBeDisabled();
    expect(request).toHaveBeenLastCalledWith('/conversations/session-a/answer', expect.objectContaining({
      method: 'POST', body: JSON.stringify({ request_id: 'review-a', tool_use_id: 'tool-a', answers: {
        type: 'plan_approval_response', version: 1, sessionId: 'session-a', requestId: 'review-a',
        toolUseId: 'tool-a', planVersion: 'version-a', action: 'approve', mode: 'ask', context: 'keep',
      } }),
    }));
    complete({ json: async () => ({ ok: true, status: 'applied', result: { mode: 'ask' } }) } as Response);
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    expect(useChatStore.getState().permissionMode).toBe('ask_permissions');
  });
});
