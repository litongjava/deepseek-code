import { useCallback, useEffect, useState } from 'react';
import { FolderOpen, FolderX, RefreshCw, X } from 'lucide-react';
import type { FsFileNode, FsTreeResponse } from '../../api';
import { listWorkspaceTree, searchWorkspaceFiles, type WorkspaceRoot, type WorkspaceScope, type WorkspaceSearch, type WorkspaceSearchMatch } from '../../api/workspaceFiles';
import { useI18n } from '../../hooks/useI18n';
import { useChatStore } from '../../stores/useChatStore';
import { useRightSidebarStore, workspaceScopeKey, type SidebarFileTab } from '../../stores/useRightSidebarStore';
import RightSidebarEmptyState from './RightSidebarEmptyState';
import FileActionsToolbar from './files/FileActionsToolbar';
import FilePreview from './files/FilePreview';
import FileTree from './files/FileTree';
import { useFileTree } from './files/useFileTree';
import { useCurrentWorkspaceScope, useWorkspaceRoots } from './files/useWorkspaceScope';

export default function RightSidebarFilesTab() {
  const currentScope = useCurrentWorkspaceScope();
  const tabs = useRightSidebarStore(state => state.tabs);
  const activeId = useRightSidebarStore(state => state.tab);
  const active = tabs.find(tab => tab.id === activeId);
  const file = active?.kind === 'file' ? active : undefined;
  const scope = file?.scope ?? currentScope;
  const scopes = new Map<string, WorkspaceScope>([[workspaceScopeKey(scope), scope]]);
  for (const tab of tabs) if (tab.kind === 'file') scopes.set(workspaceScopeKey(tab.scope), tab.scope);
  return <>{[...scopes].map(([key, item]) => <div key={key} hidden={key !== workspaceScopeKey(scope)} className="h-full">
    <WorkspaceFiles scope={item} activeFile={file && workspaceScopeKey(file.scope) === key ? file : undefined} active={key === workspaceScopeKey(scope)} />
  </div>)}</>;
}

function WorkspaceFiles({ scope, activeFile, active }: { scope: WorkspaceScope; activeFile?: SidebarFileTab; active: boolean }) {
  const { t } = useI18n();
  const { roots, loading, error, refresh } = useWorkspaceRoots(scope);
  const tabs = useRightSidebarStore(state => state.tabs);
  const openFile = useRightSidebarStore(state => state.openFile);
  const visible = useRightSidebarStore(state => state.treeVisible);
  const [diffs, setDiffs] = useState<Set<string>>(() => new Set());
  const [search, setSearch] = useState(false);
  const [revision, setRevision] = useState(0);
  const files = tabs.filter((tab): tab is SidebarFileTab => tab.kind === 'file' && workspaceScopeKey(tab.scope) === workspaceScopeKey(scope));
  const refreshAll = useCallback(() => { setRevision(value => value + 1); }, []);
  useEffect(() => useChatStore.subscribe((state, previous) => { if (previous.loading && !state.loading) refreshAll(); }), [refreshAll]);
  useEffect(() => { if (activeFile?.position) setDiffs(previous => { const next = new Set(previous); next.delete(activeFile.id); return next; }); }, [activeFile?.id, activeFile?.navigation]);
  const open = (root: WorkspaceRoot, relativePath: string, position?: WorkspaceSearchMatch) => openFile({
    scope, rootId: root.rootId, relativePath, name: relativePath.split('/').pop() || relativePath,
    path: `${root.path.replace(/[/\\]+$/, '')}/${relativePath}`, position,
  });
  if (loading) return <p role="status" className="p-4 text-sm">{t('rightSidebar.filesLoading')}</p>;
  if (error) return <div role="alert" className="p-4 text-sm text-red-600">{error}<button onClick={refresh} className="ml-2 underline">Retry</button></div>;
  if (!roots.length) return <RightSidebarEmptyState icon={FolderX} title={t('rightSidebar.filesNoWorkspaceTitle')} description={t('rightSidebar.filesNoWorkspaceDescription')} />;
  return <div className="flex h-full min-h-0 flex-col">
    <FileActionsToolbar file={activeFile} diff={!!activeFile && diffs.has(activeFile.id)} onSearch={() => setSearch(value => !value)} onDiff={() => {
      if (activeFile) setDiffs(previous => { const next = new Set(previous); if (next.has(activeFile.id)) next.delete(activeFile.id); else next.add(activeFile.id); return next; });
    }} />
    {search && <FileSearch scope={scope} roots={roots} onOpen={open} onClose={() => setSearch(false)} />}
    <div className="flex min-h-0 min-w-0 flex-1">
      <div className="min-h-0 min-w-0 flex-1" hidden={!activeFile}>
        {files.map(file => <div key={file.id} hidden={file.id !== activeFile?.id} className="h-full">
          <FilePreview file={file} active={active && file.id === activeFile?.id} diff={diffs.has(file.id)} reloadKey={revision} />
        </div>)}
      </div>
      <div data-testid="workspace-file-tree" hidden={!visible} className={`min-h-0 overflow-auto ${activeFile ? 'w-[38%] min-w-[110px] max-w-[260px] shrink-0 border-l border-deepseek-border' : 'flex-1'}`}>
        {roots.map(root => <WorkspaceTree key={`${root.rootId}:${root.path}`} scope={scope} root={root} revision={revision}
          selected={activeFile?.rootId === root.rootId ? activeFile.relativePath : null} onOpen={relative => open(root, relative)} />)}
      </div>
      {!activeFile && !visible && <p className="p-4 text-sm text-deepseek-textSecondary">File tree is hidden. Use “Show file tree” to browse files.</p>}
    </div>
  </div>;
}

function WorkspaceTree({ scope, root, revision, selected, onOpen }: { scope: WorkspaceScope; root: WorkspaceRoot; revision: number; selected: string | null; onOpen: (path: string) => void }) {
  const { t } = useI18n();
  const [truncated, setTruncated] = useState(false);
  const load = useCallback(async (path = ''): Promise<FsTreeResponse> => {
    const relative = path === root.path ? '' : path.slice(root.path.length + 1);
    const result = await listWorkspaceTree(scope, root.rootId, relative);
    setTruncated(value => value || result.truncated);
    return { success: true, path, tree: result.entries.map(entry => ({ name: entry.name, path: `${root.path}/${entry.relativePath}`, is_dir: entry.kind === 'directory', size: entry.size ?? 0 })) };
  }, [workspaceScopeKey(scope), root.rootId, root.path]);
  const tree = useFileTree(root.path, load);
  useEffect(() => { if (revision) tree.refresh(); }, [revision, tree.refresh]);
  const state = tree.dirs[root.path];
  const open = (node: FsFileNode) => onOpen(node.path.slice(root.path.length + 1));
  return <section>
    <div className="flex h-9 items-center gap-2 px-2 text-xs text-deepseek-textSecondary">
      <FolderOpen size={14} /><span className="min-w-0 flex-1 truncate" title={root.path}>{root.name}</span>
      <button type="button" title={t('rightSidebar.filesRefresh')} aria-label={t('rightSidebar.filesRefresh')} onClick={tree.refresh} className="rounded p-1 hover:bg-deepseek-hover"><RefreshCw size={13} /></button>
    </div>
    {truncated && <p className="px-2 text-xs text-deepseek-textSecondary">Directory listing was limited.</p>}
    {state?.status === 'error' ? <p role="alert" className="break-words p-2 text-xs text-red-600">{state.message}</p> : !state || (state.status === 'loading' && !state.entries) ? <p role="status" className="p-2 text-xs">{t('rightSidebar.filesLoading')}</p> : state.entries?.length === 0 ? <p className="p-3 text-xs text-deepseek-textSecondary">{t('rightSidebar.filesEmptyFolder')}</p> : <FileTree root={root.path} dirs={tree.dirs} expanded={tree.expanded} selectedPath={selected ? `${root.path}/${selected}` : null} onToggle={tree.toggle} onSetExpanded={tree.setDirExpanded} onOpenFile={open} />}
  </section>;
}

function FileSearch({ scope, roots, onOpen, onClose }: { scope: WorkspaceScope; roots: WorkspaceRoot[]; onOpen: (root: WorkspaceRoot, path: string, position: WorkspaceSearchMatch) => void; onClose: () => void }) {
  const [query, setQuery] = useState('');
  const [result, setResult] = useState<WorkspaceSearch | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  useEffect(() => {
    const controller = new AbortController();
    setResult(null); setError(''); setLoading(!!query.trim());
    const timer = window.setTimeout(() => {
      if (!query.trim()) return;
      searchWorkspaceFiles(scope, query, controller.signal).then(value => { if (!controller.signal.aborted) setResult(value); }).catch(error => { if (!controller.signal.aborted) setError(String(error.message || error)); }).finally(() => { if (!controller.signal.aborted) setLoading(false); });
    }, 250);
    return () => { window.clearTimeout(timer); controller.abort(); };
  }, [query, workspaceScopeKey(scope)]);
  return <section className="max-h-[42%] shrink-0 overflow-auto border-b border-deepseek-border p-2" aria-label="Search project contents">
    <div className="flex items-center gap-1"><input autoFocus aria-label="Search In Files" placeholder="Search project contents…" maxLength={256} value={query} onChange={event => setQuery(event.target.value)} className="min-w-0 flex-1 rounded bg-deepseek-hover px-2 py-1.5 text-xs text-deepseek-text" /><button type="button" aria-label="Close file search" title="Close file search" onClick={onClose}><X size={15} /></button></div>
    {loading && <p role="status" className="p-2 text-xs">Searching…</p>}
    {error && <p role="alert" className="p-2 text-xs text-red-600">{error}</p>}
    {result && <>
      <p className="py-2 text-xs text-deepseek-textSecondary">{result.matches.length} matches · {result.scannedFiles} files scanned{result.skippedFiles ? ` · ${result.skippedFiles} skipped` : ''}{result.truncated ? ' · Results limited' : ''}</p>
      {result.notice && <p className="text-xs text-deepseek-textSecondary">{result.notice}</p>}
      {result.matches.map((match, index) => <button type="button" key={`${match.rootId}:${match.relativePath}:${index}`} className="block w-full rounded px-2 py-1.5 text-left text-xs hover:bg-deepseek-hover" onClick={() => { const root = roots.find(root => root.rootId === match.rootId); if (root) onOpen(root, match.relativePath, match); }}>
        <span className="block truncate text-deepseek-text">{roots.find(root => root.rootId === match.rootId)?.name}/{match.relativePath}:{match.line}:{match.startColumn}</span>
        <span className="block truncate font-mono text-deepseek-textSecondary">{match.preview}</span>
      </button>)}
    </>}
  </section>;
}
