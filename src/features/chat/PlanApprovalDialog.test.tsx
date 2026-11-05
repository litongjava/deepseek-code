import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { describe, expect, it, vi } from 'vitest';
import PlanApprovalDialog, { PlanApprovalRequest } from './PlanApprovalDialog';
import { readFileSync } from 'node:fs';

const dialogCss = readFileSync('src/features/chat/PlanApprovalDialog.css', 'utf8');

vi.mock('../../components/MarkdownRenderer', () => ({ default: ({ content }: { content: string }) => <article>{content}</article> }));
const request: PlanApprovalRequest = {
  type: 'plan_approval', version: 1, sessionId: 'child', requestId: 'request', toolUseId: 'tool',
  planVersion: 'reviewed-version', plan: '# Plan\n\nKeep newlines.', planFilePath: 'plans/review.md', status: 'pending',
  choices: ['ask', 'auto', 'bypass'].flatMap(mode => ['keep', 'clear'].map(context => ({
    mode: mode as 'ask' | 'auto' | 'bypass', context: context as 'keep' | 'clear', enabled: mode !== 'bypass',
  }))),
};

describe('PlanApprovalDialog', () => {
  it('restores focus when reopened while retaining feedback and selection', () => {
    const submit = vi.fn();
    const props = { request, onSubmit: submit, onClose: vi.fn(), onOpen: vi.fn() };
    const { rerender } = render(<PlanApprovalDialog {...props} />);
    fireEvent.click(screen.getByLabelText('清理上下文，自动批准执行'));
    fireEvent.click(screen.getByRole('button', { name: '提出修改' }));
    fireEvent.change(screen.getByRole('textbox'), { target: { value: '保留这条反馈' } });
    rerender(<PlanApprovalDialog {...props} active={false} />);
    document.body.focus();
    rerender(<PlanApprovalDialog {...props} active />);
    expect(screen.getByRole('dialog')).toHaveFocus();
    expect(screen.getByRole('textbox')).toHaveValue('保留这条反馈');
    expect(screen.getByLabelText('清理上下文，自动批准执行')).toBeChecked();
    expect(submit).not.toHaveBeenCalled();
  });

  it('scopes typography overrides to the dialog without changing shared Markdown', () => {
    const element = document.createElement('style');
    element.textContent = dialogCss;
    document.head.append(element);
    const rules = Array.from(element.sheet!.cssRules).filter(rule => 'selectorText' in rule) as CSSStyleRule[];
    element.remove();
    const styleFor = (selector: string) => rules.find(rule => rule.selectorText === selector)!.style;
    const body = styleFor('.plan-review .plan-review-body .assistant-markdown.markdown-body');
    expect(body.getPropertyValue('font-family')).toContain('sans-serif');
    expect(body.getPropertyValue('font-size')).toBe('14px');
    expect(body.getPropertyValue('line-height')).toBe('24px');
    const prose = styleFor('.plan-review .assistant-markdown :is(p, ul, ol, li, blockquote, table, th, td)');
    expect(prose.getPropertyValue('font-size')).toBe('14px');
    expect(prose.getPropertyValue('line-height')).toBe('24px');
    expect(styleFor('.plan-review .assistant-markdown :is(h1, h2, h3, h4, h5, h6)').getPropertyValue('font-size')).toBe('18px');
    expect(styleFor('.plan-review .assistant-markdown :is(pre, code), .plan-review-path code').getPropertyValue('font-family')).toContain('monospace');
    expect(rules.every(rule => rule.selectorText.includes('.plan-review'))).toBe(true);
  });

  it('submits the reviewed identity and explicit mode rather than option text', async () => {
    const submit = vi.fn().mockResolvedValue(undefined);
    render(<PlanApprovalDialog request={request} onSubmit={submit} onClose={vi.fn()} onOpen={vi.fn()} />);
    fireEvent.click(screen.getByLabelText('清理上下文，自动批准执行'));
    fireEvent.click(screen.getByRole('button', { name: '批准并开始' }));
    await waitFor(() => expect(submit).toHaveBeenCalledWith(expect.objectContaining({
      sessionId: 'child', requestId: 'request', toolUseId: 'tool', planVersion: 'reviewed-version',
      mode: 'auto', context: 'clear', action: 'approve',
    })));
  });

  it('keeps feedback after a failed submit and never approves on Escape', async () => {
    const submit = vi.fn().mockRejectedValue(new Error('版本已变化'));
    const close = vi.fn();
    render(<PlanApprovalDialog request={request} onSubmit={submit} onClose={close} onOpen={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: '提出修改' }));
    fireEvent.change(screen.getByRole('textbox'), { target: { value: '补充测试' } });
    fireEvent.click(screen.getByRole('button', { name: '提交修改意见' }));
    expect(await screen.findByRole('alert')).toHaveTextContent('版本已变化');
    expect(screen.getByRole('textbox')).toHaveValue('补充测试');
    fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape' });
    expect(close).toHaveBeenCalledOnce();
    expect(submit).toHaveBeenCalledOnce();
    expect(submit.mock.calls[0][0]).toMatchObject({ action: 'revise', feedback: '补充测试' });
  });

  it('disables unauthorized choices and prevents duplicate submissions', async () => {
    let finish!: () => void;
    const submit = vi.fn(() => new Promise<void>(resolve => { finish = resolve; }));
    render(<PlanApprovalDialog request={request} onSubmit={submit} onClose={vi.fn()} onOpen={vi.fn()} />);
    expect(screen.getByLabelText(/保留上下文，bypass/)).toBeDisabled();
    const button = screen.getByRole('button', { name: '批准并开始' });
    fireEvent.click(button);
    fireEvent.click(button);
    expect(submit).toHaveBeenCalledOnce();
    expect(button).toBeDisabled();
    finish();
    await waitFor(() => expect(button).not.toBeDisabled());
  });
});
