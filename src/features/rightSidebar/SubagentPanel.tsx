import { useRef, useState } from 'react';
import { useNavigate } from 'react-router-dom';
import { isSubagentRunning, type SubagentInvocation } from '../../api/subagents';
import { useI18n } from '../../hooks/useI18n';
import MessageList from '../message-list/MessageList';
import { useSubagentTranscript } from './useSubagentTranscript';
import { conversationPageTop } from '../chat/pageConversation';

export default function SubagentPanel({ invocation, active }: { invocation: SubagentInvocation; active: boolean }) {
  const { snapshot, loading, error, retry } = useSubagentTranscript(invocation.parent_session_id, invocation.child_session_id, active);
  const navigate = useNavigate();
  const { t } = useI18n();
  const [expanded, setExpanded] = useState(new Set<number>());
  const refs = useRef(new Map<number, HTMLDivElement>());
  const status = snapshot?.status;
  return <section className="chat-font-scope chat-surface flex h-full min-h-0 flex-col" aria-label="Subagent transcript">
    <header className="shrink-0 border-b border-claude-border p-3 text-xs">
      <div className="font-medium">{snapshot?.invocation.description || invocation.description || 'Subagent'}</div>
      <div className="my-1 break-all text-claude-textSecondary">{invocation.child_session_id} · {snapshot?.invocation.mode || invocation.mode} · {status || 'Loading'}</div>
      <button type="button" disabled={!snapshot?.parent_exists} onClick={() => navigate(`/chat/${encodeURIComponent(invocation.parent_session_id)}`)} className="rounded px-2 py-1 hover:bg-claude-hover disabled:opacity-40">Go to parent conversation</button>
      <button type="button" onClick={retry} className="rounded px-2 py-1 hover:bg-claude-hover">Refresh</button>
      {snapshot && !snapshot.parent_exists && <p>Parent conversation unavailable.</p>}
      <p className="mt-1 text-claude-textSecondary">Read-only. Closing this view does not stop the agent.</p>
    </header>
    <div className="min-h-0 flex-1 overflow-auto p-3" tabIndex={0} onKeyDown={event => {
      const top = conversationPageTop(event.nativeEvent, event.currentTarget);
      if (top === null) return;
      event.preventDefault();
      event.currentTarget.scrollTo({ top, behavior: 'instant' });
    }}>
      {error && <p role="alert" className="mb-2 text-sm text-red-500">Unable to refresh transcript: {error} <button type="button" onClick={retry}>Retry</button></p>}
      {!snapshot && loading && <p role="status">Loading transcript…</p>}
      {snapshot && !snapshot.messages.length && <p role="status">{status === 'missing' ? 'Subagent conversation missing or deleted.' : isSubagentRunning(status!) ? 'Subagent is running. Waiting for messages…' : status === 'unknown' ? 'Execution status unavailable; no recorded messages.' : 'No recorded messages.'}</p>}
      {snapshot && <MessageList readOnly conversationId={invocation.child_session_id} messages={snapshot.messages} loading={isSubagentRunning(snapshot.status)} expandedMessages={expanded} editingMessageIdx={null} editingContent="" compactStatus={{ state: 'idle' }} onToggleExpand={index => setExpanded(previous => { const next = new Set(previous); if (next.has(index)) next.delete(index); else next.add(index); return next; })} messageContentRefs={refs} t={t} />}
    </div>
  </section>;
}
