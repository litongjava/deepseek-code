import { useState, useEffect, useRef } from 'react';
import { useParams, useNavigate } from 'react-router-dom';
import { FileText, ChevronDown, Trash, Pencil, Laptop, Copy, Download, Terminal, Files, Globe } from 'lucide-react';
import { updateConversation, deleteConversation, exportConversation, getConversation } from '../api';
import { tauriAPI } from '../utils/tauriAPI';
import { useProjectStore } from '../stores/useProjectStore';
import { useChatStore } from '../stores/useChatStore';
import { useConversationViewStore } from '../stores/useConversationViewStore';
import { useGeneralSettingsStore } from '../stores/useGeneralSettingsStore';
import { useI18n } from '../hooks/useI18n';
import { copyToClipboard } from '../utils/clipboard';

interface ChatHeaderProps {
  title: string;
  showArtifacts: boolean;
  documentPanelDoc: any;
  onOpenArtifacts: () => void;
  hasArtifacts: boolean;
  onTitleRename?: (newTitle: string) => void;
}

export default function ChatHeader({
  title,
  showArtifacts,
  onOpenArtifacts,
  hasArtifacts,
  onTitleRename
}: ChatHeaderProps) {
  const { id } = useParams();
  const { t } = useI18n();
  const view = useConversationViewStore(s => s.view);
  const setView = useConversationViewStore(s => s.setView);
  const showCodingView = useGeneralSettingsStore(s => s.showCodingView);
  const [copyResult, setCopyResult] = useState<{ id: string; success: boolean } | null>(null);
  useEffect(() => { setShowMenu(false); setIsEditing(false); setCopyResult(null); }, [id]);
  useEffect(() => {
    if (!copyResult) return;
    const timer = window.setTimeout(() => setCopyResult(null), 2500);
    return () => window.clearTimeout(timer);
  }, [copyResult]);
  const navigate = useNavigate();
  const [isEditing, setIsEditing] = useState(false);
  const [editTitle, setEditTitle] = useState('');
  const [showMenu, setShowMenu] = useState(false);
  const [isExporting, setIsExporting] = useState(false);
  const [workspace, setWorkspace] = useState('');
  const project = useProjectStore(s => s.projectList.find(p => p.id === s.currentProjectId));
  const currentTitle = useChatStore(s => s.conversationTitle);
  const displayTitle = currentTitle || title || 'New Chat';
  const workspacePath = project?.workspace_path || project?.path || workspace;
  const workspaceLabel = project?.name || workspacePath.split(/[/\\]/).filter(Boolean).pop();
  useEffect(() => {
    let cancelled = false;
    setWorkspace('');
    if (id) getConversation(id).then(data => { if (!cancelled) setWorkspace(data.workspace_path || ''); }).catch(() => {});
    return () => { cancelled = true; };
  }, [id, project?.id]);
  const menuRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const handleClickOutside = (event: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(event.target as Node) &&
        buttonRef.current && !buttonRef.current.contains(event.target as Node)) {
        setShowMenu(false);
      }
    };
    if (showMenu) {
      document.addEventListener('mousedown', handleClickOutside);
    }
    return () => document.removeEventListener('mousedown', handleClickOutside);
  }, [showMenu]);

  const startEditing = () => {
    setEditTitle(displayTitle);
    setIsEditing(true);
    setShowMenu(false);
  };

  const handleDelete = async () => {
    if (!id) return;
    try {
      await deleteConversation(id);
      navigate('/');
      window.dispatchEvent(new CustomEvent('conversationTitleUpdated'));
    } catch (err) {
      console.error('Failed to delete chat:', err);
    }
    setShowMenu(false);
  };

  const handleRenameSubmit = async () => {
    if (!id || !editTitle.trim()) { setIsEditing(false); return; }
    try {
      await updateConversation(id, { title: editTitle });
      useChatStore.getState().setConversationTitle(editTitle);
      onTitleRename?.(editTitle);
      window.dispatchEvent(new CustomEvent('conversationTitleUpdated'));
    } catch (err) {
      console.error('Failed to rename chat:', err);
    } finally {
      setIsEditing(false);
    }
  };

  return (
    <div data-session-header className="relative flex min-w-0 items-center gap-3 px-5 py-2 bg-claude-bg flex-shrink-0 h-[54px] z-40">
      <div className="flex min-w-0 flex-1 items-center gap-3">
      {isEditing ? (
        <input
          type="text" value={editTitle}
          onChange={(e) => setEditTitle(e.target.value)}
          onBlur={handleRenameSubmit}
          onKeyDown={(e) => { if (e.key === 'Enter') handleRenameSubmit(); else if (e.key === 'Escape') setIsEditing(false); }}
          autoFocus
          className="max-w-[60%] px-2 py-1 text-[14px] font-medium text-claude-text bg-claude-input border border-blue-500 rounded-md outline-none shadow-sm"
          style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}
        />
      ) : (
        <div className="relative flex min-w-0 flex-1 items-center gap-1" style={{ WebkitAppRegion: 'no-drag' } as React.CSSProperties}>
          <Laptop size={18} className="mr-1 shrink-0" />
          <button onClick={startEditing} title={displayTitle} className="block min-w-0 px-2 py-1.5 hover:bg-claude-btn-hover rounded-md transition-colors text-[14px] font-medium text-claude-text max-w-[280px] truncate">
            {displayTitle}
          </button>
          <button ref={buttonRef} type="button" aria-label={t('chat.sessionMenu')} aria-expanded={showMenu} onClick={() => setShowMenu(!showMenu)} className={`shrink-0 p-1 hover:bg-claude-btn-hover rounded-md transition-colors text-claude-textSecondary hover:text-claude-text ${showMenu ? 'bg-claude-btn-hover text-claude-text' : ''}`}>
            <ChevronDown size={14} />
          </button>
          {workspaceLabel && <button type="button" title={workspacePath} onClick={() => { if (workspacePath && tauriAPI.isTauri) void tauriAPI.openFolder(workspacePath); }} className="ml-3 truncate rounded-lg bg-claude-hover px-2 py-1 text-xs text-claude-textSecondary max-w-[220px]">{workspaceLabel}</button>}
          {showMenu && (
            <div ref={menuRef} className="absolute top-full left-0 mt-1 z-50 bg-claude-input border border-claude-border rounded-xl shadow-[0_4px_12px_rgba(0,0,0,0.08)] py-1.5 flex flex-col w-[200px]">
              <button onClick={(e) => { e.stopPropagation(); startEditing(); }} className="flex items-center gap-3 px-3 py-2 hover:bg-claude-hover text-left w-full transition-colors group">
                <Pencil size={16} className="text-claude-textSecondary group-hover:text-claude-text" />
                <span className="text-[13px] text-claude-text">Rename</span>
              </button>
              <button disabled={!id} onClick={async () => {
                if (!id) return;
                const copiedId = id;
                const success = await copyToClipboard(copiedId).catch(() => false);
                setCopyResult({ id: copiedId, success });
                setShowMenu(false);
              }} className="flex items-center gap-3 px-3 py-2 hover:bg-claude-hover text-left w-full text-[13px] disabled:opacity-50"><Copy size={16} />{t('chat.copySessionId')}</button>
              <div className="h-[1px] bg-claude-border my-1 mx-3" />
              <button disabled={isExporting} onClick={async () => { if (!id) return; setIsExporting(true); try { await exportConversation(id); } catch (error) { window.alert(String(error)); } finally { setIsExporting(false); setShowMenu(false); } }} className="flex items-center gap-3 px-3 py-2 hover:bg-claude-hover text-left w-full text-[13px]"><Download size={16} />{isExporting ? '导出中…' : '导出会话'}</button>
              <button onClick={(e) => { e.stopPropagation(); handleDelete(); }} className="flex items-center gap-3 px-3 py-2 hover:bg-claude-hover text-left w-full transition-colors group">
                <Trash size={16} className="text-[#B9382C]" />
                <span className="text-[13px] text-[#B9382C]">Delete</span>
              </button>
            </div>
          )}
        </div>
      )}
      </div>
      {showCodingView && <div className="flex min-w-0 shrink-0 max-w-[45%] gap-4 overflow-x-auto text-sm" role="tablist" aria-label={t('chat.sessionView')}>
        {(['chat', 'trajectory'] as const).map(tab => <button type="button" key={tab} role="tab" aria-selected={view === tab} onClick={() => setView(tab)} className={`shrink-0 py-2 ${view === tab ? 'border-b-2 border-blue-500 text-blue-500' : 'text-claude-textSecondary'}`}>{tab === 'chat' ? 'Chat' : 'Trajectory'}</button>)}
      </div>}
      {copyResult && copyResult.id === id && <div role={copyResult.success ? 'status' : 'alert'} className="absolute right-4 top-full rounded-md border border-claude-border bg-claude-input px-3 py-2 text-xs">{t(copyResult.success ? 'chat.sessionIdCopied' : 'chat.sessionIdCopyFailed')}</div>}
      <div className="flex shrink-0 items-center gap-1">
        {([{ panel: 'terminal', label: 'Terminal', Icon: Terminal }, { panel: 'files', label: 'Files', Icon: Files }, { panel: 'browser', label: 'Browser', Icon: Globe }] as const).map(({ panel, label, Icon }) => (
          <button key={panel} type="button" aria-label={label} title={label} onClick={() => window.dispatchEvent(new CustomEvent('request-open-panel', { detail: { panel } }))} className="flex h-8 w-8 items-center justify-center rounded-md text-claude-textSecondary hover:bg-claude-hover"><Icon size={18} /></button>
        ))}
        {hasArtifacts && (
          <button onClick={onOpenArtifacts} className={`w-8 h-8 flex items-center justify-center text-claude-textSecondary hover:bg-claude-btn-hover rounded-md transition-colors ${showArtifacts ? 'bg-claude-btn-hover text-claude-text' : ''}`} title="View Artifacts">
            <FileText size={18} strokeWidth={1.5} />
          </button>
        )}
      </div>
    </div>
  );
}
