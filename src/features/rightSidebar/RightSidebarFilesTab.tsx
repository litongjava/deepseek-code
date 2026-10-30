import { useCallback, useEffect, useState } from 'react';
import { AlertCircle, FolderOpen, FolderX, Loader2, RefreshCw } from 'lucide-react';
import type { FsFileNode } from '../../api';
import { useI18n } from '../../hooks/useI18n';
import { useChatStore } from '../../stores/useChatStore';
import RightSidebarEmptyState from './RightSidebarEmptyState';
import FilePreview from './files/FilePreview';
import FileTree from './files/FileTree';
import { baseName } from './files/fileUtils';
import { useFileTree } from './files/useFileTree';
import { useWorkspaceFolder } from './files/useWorkspaceFolder';

export default function RightSidebarFilesTab() {
  const { t } = useI18n();
  const workspace = useWorkspaceFolder();

  if (workspace.status === 'loading') {
    return (
      <div className="flex h-full items-center justify-center gap-2 text-[13px] text-claude-textSecondary" role="status">
        <Loader2 size={14} className="animate-spin" aria-hidden="true" />{t('rightSidebar.filesLoading')}
      </div>
    );
  }
  if (workspace.status === 'none') {
    return <RightSidebarEmptyState icon={FolderX} title={t('rightSidebar.filesNoWorkspaceTitle')} description={t('rightSidebar.filesNoWorkspaceDescription')} />;
  }
  return <WorkspaceFiles key={workspace.path} root={workspace.path} />;
}

function WorkspaceFiles({ root }: { root: string }) {
  const { t } = useI18n();
  const tree = useFileTree(root);
  const { refresh } = tree;
  const [openFile, setOpenFile] = useState<FsFileNode | null>(null);
  const [reloadKey, setReloadKey] = useState(0);
  const [focusRequest, setFocusRequest] = useState<{ path: string } | null>(null);
  const rootState = tree.dirs[root];

  const refreshAll = useCallback(() => {
    refresh();
    setReloadKey(key => key + 1);
  }, [refresh]);

  // A finished chat turn may have changed files: reload what is shown.
  useEffect(() => useChatStore.subscribe((state, prev) => {
    if (prev.loading && !state.loading) refreshAll();
  }), [refreshAll]);

  const closeFile = () => {
    if (openFile) setFocusRequest({ path: openFile.path });
    setOpenFile(null);
  };

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex h-9 flex-shrink-0 items-center gap-2 border-b border-claude-border pl-3 pr-2">
        <FolderOpen size={15} strokeWidth={1.6} className="flex-shrink-0 text-[#C6613F]" aria-hidden="true" />
        <span className="min-w-0 flex-1 truncate text-[13px] font-medium text-claude-text" title={root}>{baseName(root)}</span>
        <button
          type="button"
          onClick={refreshAll}
          aria-label={t('rightSidebar.filesRefresh')}
          title={t('rightSidebar.filesRefresh')}
          className="rounded-md p-1 text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text"
        >
          <RefreshCw size={14} className={rootState?.status === 'loading' ? 'animate-spin' : ''} aria-hidden="true" />
        </button>
      </div>
      {openFile && <FilePreview root={root} node={openFile} reloadKey={reloadKey} onBack={closeFile} />}
      {/* The tree stays mounted while a file is open so expansion and scroll survive Back. */}
      <div className="min-h-0 flex-1 overflow-auto" hidden={!!openFile}>
        {rootState?.status === 'error' ? (
          <div className="flex flex-col items-center gap-2 px-6 py-10 text-center text-[13px]" role="alert">
            <AlertCircle size={20} strokeWidth={1.5} className="text-red-600 dark:text-red-400" aria-hidden="true" />
            <p className="font-medium text-claude-text">{t('rightSidebar.filesLoadError')}</p>
            <p className="break-all text-claude-textSecondary">{rootState.message}</p>
            <button type="button" onClick={refresh} className="mt-1 rounded-md border border-claude-border px-3 py-1 text-claude-text hover:bg-claude-hover">
              {t('rightSidebar.filesRetry')}
            </button>
          </div>
        ) : !rootState || (rootState.status === 'loading' && !rootState.entries) ? (
          <div className="flex items-center gap-2 p-4 text-[13px] text-claude-textSecondary" role="status">
            <Loader2 size={14} className="animate-spin" aria-hidden="true" />{t('rightSidebar.filesLoading')}
          </div>
        ) : rootState.entries?.length === 0 ? (
          <p className="p-4 text-[13px] text-claude-textSecondary">{t('rightSidebar.filesEmptyFolder')}</p>
        ) : (
          <FileTree
            root={root}
            dirs={tree.dirs}
            expanded={tree.expanded}
            selectedPath={openFile?.path ?? null}
            focusRequest={focusRequest}
            onToggle={tree.toggle}
            onSetExpanded={tree.setDirExpanded}
            onOpenFile={setOpenFile}
          />
        )}
      </div>
    </div>
  );
}
