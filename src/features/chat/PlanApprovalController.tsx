import { useEffect, useRef, useState } from 'react';
import { request as apiRequest } from '../../api/client';
import { useToolStore } from '../../stores/useToolStore';
import PlanApprovalDialog, { PlanApprovalRequest, PlanApprovalResponse } from './PlanApprovalDialog';

export default function PlanApprovalController({ sessionId }: { sessionId: string | null }) {
  const incoming = useToolStore(state => state.askUserDialog?.planApprovalRequest);
  const [plan, setPlan] = useState<PlanApprovalRequest | null>(null);
  const [hidden, setHidden] = useState(false);
  const [opened, setOpened] = useState(false);
  const resolved = useRef(new Set<string>());
  const submitting = useRef(false);

  useEffect(() => {
    setPlan(null);
    setHidden(false);
    setOpened(false);
    if (!sessionId) return;
    const abort = new AbortController();
    let timer: ReturnType<typeof setTimeout>;
    async function refresh() {
      try {
        const response = await apiRequest(`/conversations/${encodeURIComponent(sessionId!)}/pending-questions`, { signal: abort.signal });
        const data = await response.json();
        if (abort.signal.aborted || submitting.current) return;
        const next = data.requests.find((item: { planApprovalRequest?: PlanApprovalRequest }) => item.planApprovalRequest && !resolved.current.has(item.planApprovalRequest.requestId))?.planApprovalRequest || null;
        setPlan(previous => {
          if (previous?.requestId === next?.requestId && previous?.planVersion === next?.planVersion) return previous;
          return next;
        });
      } catch { /* A disconnected read does not resolve a pending approval. */ }
      finally {
        if (!abort.signal.aborted) timer = setTimeout(refresh, 2000);
      }
    }
    void refresh();
    return () => { abort.abort(); clearTimeout(timer); };
  }, [sessionId]);

  useEffect(() => {
    if (incoming?.sessionId === sessionId && !resolved.current.has(incoming.requestId)) setPlan(incoming);
  }, [incoming, sessionId]);
  useEffect(() => { setHidden(false); setOpened(false); }, [plan?.requestId]);

  async function submit(answer: PlanApprovalResponse) {
    submitting.current = true;
    try {
      const response = await apiRequest(`/conversations/${encodeURIComponent(answer.sessionId)}/answer`, {
        method: 'POST', body: JSON.stringify({ request_id: answer.requestId, tool_use_id: answer.toolUseId, answers: answer }),
      });
      const result = await response.json();
      if (!result.ok || result.status !== 'applied') throw new Error('审批尚未应用，请查看最新请求后重试。');
      resolved.current.add(answer.requestId);
      setPlan(current => current?.requestId === answer.requestId ? null : current);
      const store = useToolStore.getState();
      if (store.askUserDialog?.request_id === answer.requestId) store.setAskUserDialog(null);
    } finally {
      submitting.current = false;
    }
  }

  if (!plan) return null;
  return <>
    {hidden && <button type="button" className="fixed bottom-24 right-6 z-50 rounded-lg bg-claude-surface p-3 shadow-lg" onClick={() => setHidden(false)}>待审阅计划</button>}
    <div style={{ display: hidden ? 'none' : undefined }}>
      <PlanApprovalDialog key={plan.requestId} request={plan} active={!hidden && !opened} onSubmit={submit} onClose={() => setHidden(true)} onOpen={() => setOpened(true)} />
    </div>
    {opened && <div className="plan-review-backdrop" style={{ zIndex: 101 }}>
      <div className="plan-review" role="dialog" aria-modal="true" aria-label="所审阅的计划文件" onKeyDown={event => {
        if (event.key === 'Escape') { event.stopPropagation(); setOpened(false); }
        if (event.key === 'Tab') { event.preventDefault(); event.currentTarget.querySelector('button')?.focus(); }
      }}>
        <header className="plan-review-header"><strong>所审阅的计划文件（只读快照）</strong><button autoFocus type="button" onClick={() => setOpened(false)}>关闭</button></header>
        <div className="plan-review-body"><code>{plan.planFilePath}</code><pre style={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>{plan.plan}</pre></div>
      </div>
    </div>}
  </>;
}
