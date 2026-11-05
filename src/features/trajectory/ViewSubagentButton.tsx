import { useEffect, useState } from 'react';
import { listSubagents, type SubagentInvocation } from '../../api/subagents';
import { openSubagentPanel } from '../rightSidebar/useRightSidebarController';

export default function ViewSubagentButton({ conversationId, toolId, live }: { conversationId?: string | null; toolId?: string; live: boolean }) {
  const [requested, setRequested] = useState(false);
  const [invocations, setInvocations] = useState<SubagentInvocation[]>([]);
  const [error, setError] = useState('');
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    if (!requested || !conversationId || !toolId) return;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const resolve = async () => {
      try {
        const matches = await listSubagents(conversationId, toolId, controller.signal);
        if (controller.signal.aborted) return;
        setInvocations(matches);
        if (matches.length === 1) { openSubagentPanel(matches[0]); setRequested(false); }
        else if (!matches.length) {
          setError(live ? 'Waiting for subagent association…' : 'Subagent association unavailable for this historical call.');
          if (live) timer = setTimeout(resolve, 2000);
        } else setError('');
      } catch (err) {
        if (!controller.signal.aborted) setError(err instanceof Error ? err.message : String(err));
      }
    };
    void resolve();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [requested, conversationId, toolId, live, revision]);
  return <div className="my-1 text-xs">
    <button type="button" disabled={!conversationId || !toolId} onClick={() => { setRequested(true); setRevision(value => value + 1); setError(''); }} className="rounded-full border border-claude-border px-2 py-1 hover:bg-claude-hover disabled:opacity-40">View subagent</button>
    {requested && error && <p role="status">{error}</p>}
    {requested && invocations.length > 1 && invocations.map(item => <button type="button" key={item.attempt_id} onClick={() => { openSubagentPanel(item); setRequested(false); }} className="block p-1">{item.description || 'Subagent'} · {item.child_session_id} · {item.status}</button>)}
  </div>;
}
