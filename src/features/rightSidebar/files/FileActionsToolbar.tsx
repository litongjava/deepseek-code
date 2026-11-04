import { useEffect, useState } from 'react';
import { ChevronDown, Copy, FileSearch, Folders, FolderOpen } from 'lucide-react';
import { copyToClipboard } from '../../../utils/clipboard';
import { getWorkspaceFileCapabilities, openWorkspaceFile, saveWorkspaceFileAs, type WorkspaceFileCapabilities, type WorkspaceFileAction } from '../../../api/workspaceFiles';
import { type SidebarFileTab, useRightSidebarStore } from '../../../stores/useRightSidebarStore';

export default function FileActionsToolbar({ file, diff, onDiff, onSearch }: {
  file?: SidebarFileTab; diff: boolean; onDiff: () => void; onSearch: () => void;
}) {
  const visible = useRightSidebarStore(state => state.treeVisible);
  const toggleTree = useRightSidebarStore(state => state.toggleTree);
  const [caps, setCaps] = useState<WorkspaceFileCapabilities | null>(null);
  const [menu, setMenu] = useState(false);
  const [busy, setBusy] = useState(false);
  const [feedback, setFeedback] = useState<{ error: boolean; text: string } | null>(null);
  useEffect(() => {
    let current = true;
    setCaps(null); setMenu(false); setFeedback(null);
    if (file) getWorkspaceFileCapabilities(file.scope, file).then(value => { if (current) setCaps(value); }).catch(error => {
      if (current) setFeedback({ error: true, text: String(error.message || error) });
    });
    return () => { current = false; };
  }, [file?.id]);
  useEffect(() => {
    if (!feedback || feedback.error) return;
    const timer = window.setTimeout(() => setFeedback(null), 2500);
    return () => window.clearTimeout(timer);
  }, [feedback]);
  const act = async (action: WorkspaceFileAction | 'save-as', distribution?: string) => {
    if (!file || busy) return;
    setBusy(true); setMenu(false); setFeedback(null);
    try {
      if (action === 'save-as') {
        const result = await saveWorkspaceFileAs(file.scope, file);
        if (result.status !== 'cancelled') setFeedback({ error: false, text: result.status === 'saved' ? 'File saved.' : 'Source and destination are the same file; nothing changed.' });
      } else await openWorkspaceFile(file.scope, file, action, distribution);
    } catch (error) { setFeedback({ error: true, text: error instanceof Error ? error.message : String(error) }); }
    finally { setBusy(false); }
  };
  const button = 'rounded-md p-1.5 text-claude-textSecondary hover:bg-claude-hover disabled:opacity-40';
  const disabledReason = caps?.reason || 'Native file actions are unavailable.';
  return <div className="shrink-0 border-b border-claude-border">
    {file && <div className="flex min-w-0 items-center gap-1 px-3 pt-2"><span className="min-w-0 flex-1 truncate font-mono text-xs text-claude-textSecondary" title={file.path}>{file.relativePath}</span><button type="button" title="Copy path" aria-label="Copy path" className={button} onClick={async () => { const copied = await copyToClipboard(file.path); setFeedback({ error: !copied, text: copied ? 'Path copied.' : 'Could not copy path.' }); }}><Copy size={13} /></button></div>}
    <div className="relative flex items-center gap-1 px-2 py-1" aria-label="File actions">
      <button type="button" className={button} title="Search In Files" aria-label="Search In Files" onClick={onSearch}><FileSearch size={16} /></button>
      <button type="button" className={`${button} ${diff ? 'bg-claude-hover' : ''}`} title="View file diff" aria-label="View file diff" disabled={!file} aria-pressed={diff} onClick={onDiff}>
        <svg width="16" height="16" viewBox="0 0 16 16" aria-hidden="true">
          <rect x="1" y="1" width="14" height="6" rx="1" className="fill-red-500" />
          <rect x="1" y="9" width="14" height="6" rx="1" className="fill-green-600" />
          <path d="M5 4h6M5 12h6M8 10v4" stroke="white" strokeWidth="1.25" />
        </svg>
      </button>
      <button type="button" className={button} title={visible ? 'Hide file tree' : 'Show file tree'} aria-label={visible ? 'Hide file tree' : 'Show file tree'} aria-pressed={visible} onClick={toggleTree}><Folders size={16} /></button>
      <div className="ml-auto flex shrink-0 items-center rounded-md bg-claude-hover">
        <button type="button" disabled={busy || !caps?.reveal} title={caps?.reveal ? 'Reveal file in system file manager' : disabledReason} onClick={() => void act('reveal')} className={`${button} flex items-center gap-1 text-xs`}><FolderOpen size={15} />Open</button>
        <button type="button" className={button} disabled={!file || busy} aria-label="Choose opening method" title="Choose opening method" aria-expanded={menu} onClick={() => setMenu(value => !value)}><ChevronDown size={14} /></button>
      </div>
      {menu && <>
        <button type="button" tabIndex={-1} aria-label="Dismiss opening methods" className="fixed inset-0 z-30 cursor-default" onClick={() => setMenu(false)} />
        <div role="menu" aria-label="Open file with" onKeyDown={event => { if (event.key === 'Escape') setMenu(false); }} className="absolute right-2 top-full z-40 min-w-48 rounded-lg border border-claude-border bg-claude-bg p-1 shadow-lg">
          {([
            ['Default app', 'default-app', caps?.defaultApp], ['Terminal', 'terminal', caps?.terminal],
            ['Open in folder', 'reveal', caps?.reveal], ['Save as…', 'save-as', caps?.saveAs],
          ] as const).map(([label, action, enabled], index) => <div key={action}>
            {index === 2 && (caps?.wslDistributions.length ? caps.wslDistributions.map(distribution => <button key={distribution} role="menuitem" type="button" className="block w-full rounded px-3 py-2 text-left text-xs hover:bg-claude-hover" onClick={() => void act('wsl-terminal', distribution)}>WSL · {distribution}</button>) : <button type="button" role="menuitem" disabled title={caps?.reason || 'No usable WSL distribution on this platform.'} className="block w-full px-3 py-2 text-left text-xs opacity-40">WSL — unavailable</button>)}
            <button type="button" role="menuitem" disabled={!enabled} title={enabled ? (action === 'terminal' ? 'Open external system terminal at this file’s folder' : label) : disabledReason} onClick={() => void act(action)} className="block w-full rounded px-3 py-2 text-left text-xs hover:bg-claude-hover disabled:opacity-40">{label}</button>
          </div>)}
        </div>
      </>}
    </div>
    {feedback && <p role={feedback.error ? 'alert' : 'status'} className={`px-3 pb-2 text-xs ${feedback.error ? 'text-red-600' : 'text-claude-textSecondary'}`}>{feedback.text}</p>}
  </div>;
}
