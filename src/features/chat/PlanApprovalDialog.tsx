import { useEffect, useId, useRef, useState } from 'react';
import MarkdownRenderer from '../../components/MarkdownRenderer';
import { copyToClipboard } from '../../utils/clipboard';
import './PlanApprovalDialog.css';

export interface PlanApprovalRequest {
  type: 'plan_approval';
  version: 1;
  sessionId: string;
  requestId: string;
  toolUseId: string;
  planVersion: string;
  plan: string;
  planFilePath: string;
  status: string;
  choices: Array<{ mode: 'ask' | 'auto' | 'bypass'; context: 'keep' | 'clear'; enabled: boolean; disabledReason?: string }>;
}

export interface PlanApprovalResponse {
  type: 'plan_approval_response';
  version: 1;
  sessionId: string;
  requestId: string;
  toolUseId: string;
  planVersion: string;
  action: 'approve' | 'revise';
  mode?: 'ask' | 'auto' | 'bypass';
  context?: 'keep' | 'clear';
  feedback?: string;
}

interface Props {
  request: PlanApprovalRequest;
  active?: boolean;
  onSubmit: (response: PlanApprovalResponse) => Promise<void>;
  onClose: () => void;
  onOpen: (path: string) => void | Promise<void>;
}

export default function PlanApprovalDialog({ request, active = true, onSubmit, onClose, onOpen }: Props) {
  const titleId = useId();
  const descriptionId = useId();
  const panel = useRef<HTMLDivElement>(null);
  const [choice, setChoice] = useState('ask:keep');
  const [revising, setRevising] = useState(false);
  const [feedback, setFeedback] = useState('');
  const [pending, setPending] = useState(false);
  const submitting = useRef(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const selected = request.choices.find(item => `${item.mode}:${item.context}` === choice);
  const inactive = request.status !== 'pending';

  useEffect(() => {
    if (!active) return;
    const previous = document.activeElement as HTMLElement | null;
    panel.current?.focus();
    return () => { if (previous?.isConnected) previous.focus(); };
  }, [active]);

  async function submit() {
    if (submitting.current || inactive || (!revising && !selected?.enabled) || (revising && !feedback.trim())) return;
    submitting.current = true;
    setPending(true);
    setError('');
    try {
      await onSubmit({
        type: 'plan_approval_response', version: 1,
        sessionId: request.sessionId, requestId: request.requestId,
        toolUseId: request.toolUseId, planVersion: request.planVersion,
        action: revising ? 'revise' : 'approve',
        ...(revising ? { feedback: feedback.trim() } : { mode: selected!.mode, context: selected!.context }),
      });
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : '提交失败，请重试。');
    } finally {
      submitting.current = false;
      setPending(false);
    }
  }

  async function copy(text: string) {
    if (await copyToClipboard(text)) setNotice('已复制');
    else setError('复制失败，请重试。');
  }

  return <div className="plan-review-backdrop">
    <div ref={panel} className="plan-review" role="dialog" aria-modal="true" aria-labelledby={titleId}
      aria-describedby={descriptionId} tabIndex={-1} onKeyDown={event => {
        event.stopPropagation();
        if (event.key === 'Escape') { event.preventDefault(); if (!pending) onClose(); }
        if (event.key === 'Tab') {
          const focusable = Array.from(panel.current?.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), textarea:not(:disabled), a[href], summary') || []);
          const first = focusable[0];
          const last = focusable[focusable.length - 1];
          if (event.shiftKey && (document.activeElement === first || document.activeElement === panel.current)) {
            event.preventDefault(); last?.focus();
          } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === panel.current)) {
            event.preventDefault(); first?.focus();
          }
        }
      }}>
      <header className="plan-review-header">
        <div><h2 id={titleId}>审阅执行计划</h2><p id={descriptionId}>批准后结束规划阶段，并按所选方式开始实施</p></div>
        <button type="button" aria-label="收起计划审批" disabled={pending} onClick={onClose}>×</button>
      </header>
      <section className="plan-review-body" aria-label="计划正文">
        <div className="plan-review-toolbar">
          <strong>执行计划</strong>
          <button type="button" onClick={() => void copy(request.plan)}>复制计划</button>
          <button type="button" disabled={!request.planFilePath} onClick={() => {
            void Promise.resolve().then(() => onOpen(request.planFilePath)).catch(cause => setError(String(cause)));
          }}>打开</button>
        </div>
        {request.planFilePath && <details className="plan-review-path"><summary>计划文件路径</summary>
          <code>{request.planFilePath}</code><button type="button" onClick={() => void copy(request.planFilePath)}>复制路径</button>
        </details>}
        <MarkdownRenderer content={request.plan} />
      </section>
      <footer className="plan-review-footer">
        <fieldset disabled={pending || inactive}><legend>执行方式</legend>
          <div className="plan-review-choices">{request.choices.map(item => {
            const value = `${item.mode}:${item.context}`;
            return <label key={value} title={item.disabledReason}>
              <input type="radio" name={titleId} value={value} checked={choice === value} disabled={!item.enabled}
                onChange={() => setChoice(value)} />
              <span>{item.context === 'keep' ? '保留上下文' : '清理上下文'}，{item.mode === 'ask' ? '逐次审批执行' : item.mode === 'auto' ? '自动批准执行' : 'bypass 执行'}
                {!item.enabled && <small>{item.disabledReason || '当前会话未授权此方式'}</small>}</span>
            </label>;
          })}</div>
        </fieldset>
        {selected?.context === 'clear' && <p className="plan-review-help">清理模型工作上下文，保留已保存的会话历史，并按批准的计划继续。</p>}
        {revising && <label className="plan-review-feedback">修改意见<textarea autoFocus rows={3} value={feedback}
          disabled={pending || inactive} onChange={event => setFeedback(event.target.value)} /></label>}
        {inactive && <p role="status">此审批请求已完成或失效，请查看最新请求。</p>}
        {error && <p role="alert">{error}</p>}
        <span role="status" className="plan-review-help">{notice}</span>
        <div className="plan-review-actions">
          <button type="button" disabled={pending || inactive} onClick={() => setRevising(!revising)}>{revising ? '返回审批' : '提出修改'}</button>
          <button type="button" className="plan-review-primary" disabled={pending || inactive || (revising ? !feedback.trim() : !selected?.enabled)} onClick={() => void submit()}>
            {pending ? '提交中…' : revising ? '提交修改意见' : '批准并开始'}
          </button>
        </div>
      </footer>
    </div>
  </div>;
}
