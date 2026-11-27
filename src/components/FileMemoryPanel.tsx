import { useEffect, useMemo, useRef, useState } from 'react';
import { fileMemoryApi, type FileMemoryItem, type FileMemoryList } from '../api/fileMemory';
import { useChatStore } from '../stores/useChatStore';
import { useProjectStore } from '../stores/useProjectStore';
import { MemoryPanel } from './MemoryPanel';

const template = '---\nname: \ndescription: \nmetadata:\n  type: user\n---\n';
const field = 'w-full rounded border border-deepseek-border bg-deepseek-bg p-2';
export function FileMemoryPanel() {
  const session = useChatStore(s => s.conversationId);
  const project = useProjectStore(s => s.currentProjectId);
  const scope = useMemo(() => session ? { session_id: session } : project ? { project_id: project } : null, [session, project]);
  const epoch = useRef(0);
  const [legacy, setLegacy] = useState(false);
  const [list, setList] = useState<FileMemoryList | null>(null);
  const [editing, setEditing] = useState<FileMemoryItem | null>(null);
  const [name, setName] = useState('');
  const [content, setContent] = useState(template);
  const [query, setQuery] = useState('');
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const [revision, setRevision] = useState(0);
  useEffect(() => {
    const request = ++epoch.current;
    setList(null); setEditing(null); setName(''); setContent(template); setError('');
    if (!scope || legacy) return;
    setBusy(true);
    fileMemoryApi.list(scope, query).then(value => { if (epoch.current === request) setList(value); })
      .catch(e => { if (epoch.current === request) setError(String(e.message || e)); })
      .finally(() => { if (epoch.current === request) setBusy(false); });
    return () => { ++epoch.current; };
  }, [scope, query, revision, legacy]);
  const mutate = async (action: () => Promise<unknown>) => {
    const request = epoch.current;
    setBusy(true); setError('');
    try { await action(); if (request === epoch.current) setRevision(v => v + 1); }
    catch (e) { if (request === epoch.current) setError(`${String(e instanceof Error ? e.message : e)}. Refresh before editing again if this memory changed.`); }
    finally { if (request === epoch.current) setBusy(false); }
  };
  return <section className="space-y-4 p-6 text-deepseek-text">
    <div className="flex gap-4"><h2>Project memories</h2><button onClick={() => setLegacy(v => !v)}>{legacy ? 'Back to project memories' : 'Legacy archive / remote settings'}</button></div>
    {legacy ? <><p>Unassigned legacy entries are preserved here and do not enter local project recall. Dream is paused after file memory activation.</p><MemoryPanel /></> : !scope ? <p>Select a trusted project or open a session to manage its memories.</p> : <>
      <p>Memories for {session ? 'the current session’s project or private workspace' : 'the selected project'}. The index is loaded at the start of each conversation.</p>
      <label>Search memories<input className={field} value={query} onChange={e => setQuery(e.target.value)} /></label>
      <button disabled={busy} onClick={() => setRevision(v => v + 1)}>Refresh</button>
      {error && <p role="alert">{error}</p>}
      {list && <><p>{list.items.length} memories · {list.omitted} index entries omitted from context</p>
        {list.diagnostics.map(d => <p role="alert" key={d.file}>{d.file}: {d.message}</p>)}
        <ul>{list.items.map(item => <li key={item.id} className="my-3"><strong>{item.name}</strong> ({item.type}) — {item.description}
          <button disabled={busy} onClick={() => { setEditing(item); setName(item.id); setContent(item.content); }}>Edit {item.name}</button>
          <button disabled={busy} onClick={() => { if (window.confirm(`Delete memory “${item.name}” from this scope?`)) void mutate(() => fileMemoryApi.remove(scope, item)); }}>Delete {item.name}</button>
        </li>)}</ul></>}
      <form onSubmit={e => { e.preventDefault(); void mutate(() => fileMemoryApi.save(scope, name, content, editing?.version)); }} className="space-y-3">
        <h3>{editing ? 'Edit memory' : 'New memory'}</h3>
        <label>Filename<input required className={field} value={name} disabled={busy || !!editing} pattern="[a-z0-9][a-z0-9_-]*\.md" placeholder="preferences.md" onChange={e => setName(e.target.value)} /></label>
        <label>Markdown<textarea required className={field} rows={12} value={content} disabled={busy} onChange={e => setContent(e.target.value)} /></label>
        <p>Include name, description, and metadata.type (user, feedback, project, reference). Feedback and project memories also need Why and How to apply sections.</p>
        <button disabled={busy}>Save memory</button>
        {editing && <button type="button" onClick={() => { setEditing(null); setName(''); setContent(template); }}>Cancel edit</button>}
      </form>
    </>}
  </section>;
}
