import SubagentPanel from './SubagentPanel';
import React, { useEffect, useRef, useState } from 'react';
import { Plus, X } from 'lucide-react';
import { useI18n } from '../../hooks/useI18n';
import { RIGHT_SIDEBAR_TABS, useRightSidebarStore } from '../../stores/useRightSidebarStore';
import type { WorkspaceScope } from '../../api/workspaceFiles';
import TerminalPanel, { type TerminalPanelHandle } from '../../components/TerminalPanel';
import RightSidebarFilesTab from './RightSidebarFilesTab';
import RightSidebarBrowserTab from './RightSidebarBrowserTab';
import { useCurrentWorkspaceScope, useWorkspaceRoots } from './files/useWorkspaceScope';

export const RIGHT_SIDEBAR_ID = 'right-sidebar';

const WorkspaceTerminal = React.forwardRef<TerminalPanelHandle, { scope: WorkspaceScope; active: boolean; onClose: () => void }>(({ scope, active, onClose }, ref) => {
  const [boundScope] = useState(scope);
  const workspace = useWorkspaceRoots(boundScope);
  const [folder, setFolder] = useState<{ cwd?: string } | null>(null);
  useEffect(() => {
    if (!folder && !workspace.loading && !workspace.error) setFolder({ cwd: workspace.roots[0]?.path });
  }, [folder, workspace.loading, workspace.error, workspace.roots]);
  if (folder) return <TerminalPanel ref={ref} active={active} cwd={folder.cwd} onClose={onClose} />;
  if (workspace.error) return <p role="alert" className="p-4 text-xs text-red-600">{workspace.error}</p>;
  return <p role="status" className="p-4 text-xs">Resolving terminal folder…</p>;
});

export default function RightSidebar({ onClose, active = true }: { onClose: () => void; active?: boolean }) {
  const { t } = useI18n();
  const { tab, tabs, setTab, openTab, closeTab } = useRightSidebarStore();
  const terminal = useRef<TerminalPanelHandle>(null);
  const tabRefs = useRef(new Map<string, HTMLButtonElement>());
  const [adding, setAdding] = useState(false);
  const [closing, setClosing] = useState(false);
  const [error, setError] = useState('');
  const scope = useCurrentWorkspaceScope();
  const hasTerminal = tabs.some(item => item.kind === 'terminal');
  const hasFiles = tabs.some(item => item.kind === 'file' || item.kind === 'files');
  const fileActive = tab === 'files' || tabs.some(item => item.kind === 'file' && item.id === tab);
  const close = async (id: string) => {
    if (closing) return;
    if (id === 'terminal' && terminal.current) {
      setClosing(true); setError('');
      try { if (!await terminal.current.requestClose()) return; }
      catch (error) { setError(error instanceof Error ? error.message : String(error)); return; }
      finally { setClosing(false); }
    }
    closeTab(id);
  };
  const keyboard = (event: React.KeyboardEvent, id: string) => {
    const index = tabs.findIndex(item => item.id === id);
    if (event.key === 'Delete') { event.preventDefault(); void close(id); return; }
    const next = event.key === 'ArrowRight' ? (index + 1) % tabs.length : event.key === 'ArrowLeft' ? (index + tabs.length - 1) % tabs.length : event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 : null;
    if (next === null) return;
    event.preventDefault(); setTab(tabs[next].id); tabRefs.current.get(tabs[next].id)?.focus();
  };
  return <aside id={RIGHT_SIDEBAR_ID} aria-label={t('rightSidebar.panel')} className="flex h-full w-full min-w-0 flex-col bg-deepseek-bg">
    <div className="relative flex h-[44px] shrink-0 items-center gap-1 px-2">
      <div role="tablist" aria-label={t('rightSidebar.tabs')} className="flex min-w-0 flex-1 items-center gap-1 overflow-x-auto">
        {tabs.map(item => {
          const selected = item.id === tab;
          const title = item.kind === 'subagent' ? (item.invocation.description || 'Subagent') : item.kind === 'file' ? item.relativePath : item.kind === 'terminal' ? 'Terminal' : t(`rightSidebar.${item.kind}`);
          const duplicate = item.kind === 'file' && tabs.some(other => other.kind === 'file' && other.id !== item.id && other.name === item.name);
          return <div key={item.id} className={`flex shrink-0 items-center rounded-md ${selected ? 'bg-deepseek-btn-hover text-deepseek-text' : 'text-deepseek-textSecondary'}`}>
            <button type="button" role="tab" ref={node => { if (node) tabRefs.current.set(item.id, node); else tabRefs.current.delete(item.id); }}
              id={`${RIGHT_SIDEBAR_ID}-tab-${item.id}`} aria-controls={`${RIGHT_SIDEBAR_ID}-content`} aria-selected={selected} tabIndex={selected ? 0 : -1}
              title={item.kind === 'file' ? item.path : title} onClick={() => setTab(item.id)} onKeyDown={event => keyboard(event, item.id)}
              className="max-w-[190px] truncate rounded-md px-2 py-1.5 text-xs hover:bg-deepseek-hover focus-visible:ring-2 focus-visible:ring-blue-500">
              {item.kind === 'file' ? (duplicate ? `${item.rootId}/${item.relativePath}` : item.name) : title}
            </button>
            <button type="button" aria-label={`Close ${title}`} title={`Close ${title}`} disabled={closing} onClick={() => void close(item.id)} className="mr-1 rounded p-0.5 hover:bg-deepseek-hover"><X size={12} /></button>
          </div>;
        })}
      </div>
      <button type="button" title="Open panel tab" aria-label="Open panel tab" aria-expanded={adding} onClick={() => setAdding(value => !value)} className="rounded p-1 text-deepseek-textSecondary hover:bg-deepseek-hover"><Plus size={16} /></button>
      <button type="button" title={t('rightSidebar.close')} aria-label={t('rightSidebar.close')} onClick={onClose} className="rounded p-1 text-deepseek-textSecondary hover:bg-deepseek-hover"><X size={16} /></button>
      {adding && <div className="absolute right-7 top-full z-30 rounded-lg border border-deepseek-border bg-deepseek-bg p-1 shadow-lg">
        {RIGHT_SIDEBAR_TABS.map(id => <button key={id} type="button" className="block w-full rounded px-4 py-2 text-left text-xs hover:bg-deepseek-hover" onClick={() => { openTab(id); setAdding(false); }}>{id === 'terminal' ? 'Terminal' : t(`rightSidebar.${id}`)}</button>)}
      </div>}
    </div>
    {error && <p role="alert" className="px-3 text-xs text-red-600">{error}</p>}
    <div role="tabpanel" id={`${RIGHT_SIDEBAR_ID}-content`} aria-labelledby={tab ? `${RIGHT_SIDEBAR_ID}-tab-${tab}` : undefined} className="min-h-0 flex-1 overflow-hidden">
      {!tabs.length && <div className="flex h-full items-center justify-center gap-2">{RIGHT_SIDEBAR_TABS.map(id => <button type="button" key={id} onClick={() => { openTab(id); }} className="rounded-md bg-deepseek-hover px-3 py-2 text-xs">{id === 'terminal' ? 'Terminal' : t(`rightSidebar.${id}`)}</button>)}</div>}
      {tabs.map(item => item.kind === 'subagent' && <div key={item.id} hidden={tab !== item.id} className="h-full"><SubagentPanel invocation={item.invocation} active={active && tab === item.id} /></div>)}
      {hasFiles && <div hidden={!fileActive} className="h-full"><RightSidebarFilesTab /></div>}
      {tabs.some(item => item.kind === 'browser') && <div hidden={tab !== 'browser'} className="h-full"><RightSidebarBrowserTab /></div>}
      {hasTerminal && <div hidden={tab !== 'terminal'} className="h-full">
        <WorkspaceTerminal ref={terminal} scope={scope} active={active && tab === 'terminal'} onClose={() => closeTab('terminal')} />
      </div>}
    </div>
  </aside>;
}
