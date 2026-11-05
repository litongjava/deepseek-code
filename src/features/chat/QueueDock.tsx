import { useEffect, useState } from 'react';
import { MessageSquare } from 'lucide-react';
import { useBusySendStore } from '../../stores/useBusySendStore';
import type { QueueItem } from '../../api/queue';
import type { UserContent } from '../../utils/userContent';
import { embeddedImageUrl } from '../../utils/userContent';
import './QueueDock.css';

function Preview({ content }: { content: UserContent }) {
  if (typeof content === 'string') return <span className="queue-preview" title={content}>{content}</span>;
  const text = content.filter(block => block.type === 'text').map(block => block.text).join('\n');
  const attachments = content.filter(block => block.type !== 'text');
  return <span className="queue-preview queue-content">
    {text && <span className="queue-preview-text" title={text}>{text}</span>}
    {attachments.length > 0 && <span className="queue-attachments">{attachments.map((block, index) => {
    const url = embeddedImageUrl(block);
    return <span key={index} className="queue-attachment">
      {url && <img src={url} alt={block.fileName || 'Queued image'} />}
      <span className="queue-file-name" title={block.fileName}>{block.fileName || 'Image attachment'}</span>
      {typeof block.size === 'number' && <span className="queue-status">{block.size < 1024 ? `${block.size} B` : `${(block.size / 1024).toFixed(1)} KB`}</span>}
    </span>;
  })}</span>}</span>;
}
function QueueRow({ session, item, busy, canSteer }: { session: string; item: QueueItem; busy: boolean; canSteer: boolean }) {
  const [draft, setDraft] = useState<string | null>(null);
  const [conflict, setConflict] = useState(false);
  const [editRevision, setEditRevision] = useState(item.revision);
  const textOnly = typeof item.content === 'string' || item.content.every(block => block.type === 'text');
  const text = typeof item.content === 'string' ? item.content : item.content.filter(b => b.type === 'text').map(b => b.text).join('\n');
  const editable = item.state === 'queued' && !busy;
  const save = async () => {
    if (draft === null || !draft.trim() || !editable) return;
    if (await useBusySendStore.getState().mutate(session, { ...item, revision: editRevision }, 'edit', draft)) { setDraft(null); setConflict(false); }
    else setConflict(true);
  };
  return <div className="queue-row">
    <MessageSquare size={14} className="queue-icon" aria-hidden="true" />
    {draft !== null ? <div className="queue-editor">
      <textarea aria-label="Edit queued message" value={draft} disabled={!editable} autoFocus onChange={e => setDraft(e.target.value)} onKeyDown={e => {
        if (e.nativeEvent.isComposing || e.keyCode === 229) return;
        if (e.key === 'Escape') { e.preventDefault(); setDraft(null); setConflict(false); }
        if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); void save(); }
      }} />
      {(conflict || !editable) && <span role="alert">Queue changed; draft retained. Copy it before closing.</span>}
      <button onClick={() => { setDraft(null); setConflict(false); }}>Cancel edit</button>
    </div> : <Preview content={item.content} />}
    <span className="queue-status">{busy ? 'Saving…' : item.state === 'claimed' ? 'Delivering…' : item.mode === 'steer' ? 'Steer' : ''}</span>
    {draft === null && <button disabled={!editable || !textOnly} title={!textOnly ? 'Attachment messages cannot be edited' : 'Edit queued message'} aria-label="Edit queued message" onClick={() => { setEditRevision(item.revision); setDraft(text); }}>Edit</button>}
    <button disabled={!editable || !canSteer || item.mode === 'steer'} title={!canSteer ? 'Steer requires an active execution and an unpaused queue' : 'Guide the current execution'} onClick={() => void useBusySendStore.getState().mutate(session, item, 'steer')}>Steer</button>
    <button disabled={!editable} aria-label="Delete queued message" onClick={() => void useBusySendStore.getState().mutate(session, item, 'delete')}>×</button>
  </div>;
}
export default function QueueDock({ session, executing = false }: { session: string; executing?: boolean }) {
  const queue = useBusySendStore(state => state.queues[session]);
  const [expanded, setExpanded] = useState(false);
  useEffect(() => {
    let disposed = false;
    let timer: ReturnType<typeof setTimeout>;
    const refresh = async () => {
      await useBusySendStore.getState().refresh(session);
      if (!disposed) timer = setTimeout(refresh, 1500);
    };
    void refresh();
    const onFocus = () => { void useBusySendStore.getState().refresh(session); };
    window.addEventListener('focus', onFocus);
    return () => { disposed = true; clearTimeout(timer); window.removeEventListener('focus', onFocus); };
  }, [session]);
  const items = queue?.items.filter(item => item.state === 'queued' || item.state === 'claimed') || [];
  const pending = queue?.pending || [];
  const count = items.length + pending.length;
  if (!count && !queue?.error) return null;
  return <section className="queue-dock" aria-label="Queued messages">
    {count > 1 && <button className="queue-header" aria-expanded={expanded} onClick={() => setExpanded(value => !value)}>{expanded ? '▾' : '▸'} Queued ({count})</button>}
    {queue?.paused && count > 0 && <button onClick={() => void useBusySendStore.getState().resume(session)}>Queue paused · Resume</button>}
    {queue?.error && <div role="alert">{queue.error}</div>}
    {(count <= 1 || expanded) && <div className="queue-rows">
      {items.map(item => <QueueRow key={item.id} session={session} item={item} busy={queue?.busy.includes(item.id) || false} canSteer={executing && !queue?.paused} />)}
      {pending.map(item => <div className="queue-row" key={item.requestId}>
        <MessageSquare size={14} className="queue-icon" aria-hidden="true" />
        <Preview content={item.content} />
        <span className="queue-status">{item.status === 'sending' ? 'Sending…' : 'Failed'}</span>
        {item.error && <span role="alert" title={item.error}>{item.error}</span>}
        {item.status === 'failed' && <button onClick={() => void useBusySendStore.getState().retry(session, item.requestId)}>Retry</button>}
      </div>)}
    </div>}
  </section>;
}
