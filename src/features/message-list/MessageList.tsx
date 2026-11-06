import CopyButton from '../trajectory/CopyButton';
import React from 'react';
import UserContentBlocks from './UserContentBlocks';
import { turnLabel } from './turnTiming';
import { contentBlocks, hasUnavailableImages, type UserContent } from '../../utils/userContent';
import WorkDetails from '../trajectory/WorkDetails';
import { messageSteps } from '../trajectory/model';
import ResponseFooter from './ResponseFooter';
import { useGeneralSettingsStore } from '../../stores/useGeneralSettingsStore';
import { openChatLink } from '../../utils/chatLinks';
import {
  RotateCcw,
  Pencil,
  Globe,
  Info,
  Loader2,
} from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import ClaudeLogo from '../../components/ClaudeLogo';
import CodeExecution from '../../components/CodeExecution';
import SearchProcess from '../../components/SearchProcess';
import DocumentCreationProcess from '../../components/DocumentCreationProcess';
import DiffViewer from '../../components/DiffViewer';
import DocumentCard, { DocumentInfo } from '../../components/DocumentCard';
import MessageAttachments from '../../components/MessageAttachments';
import { IconResearch } from '../../components/Icons';
import CompactingStatus from '../compact/CompactingStatus';
import { PendingFile } from '../../components/FileUploadPreview';
import {
  extractTextContent,
  formatMessageTime,
  withAuthToken,
  normalizeMessageDocuments,
  normalizeDocumentDrafts,
} from '../../utils/messageHelpers';

// 草稿存储：在切换对话、打开设置页面时保留输入内容和附件
export const draftsStore = new Map<string, { text: string; files: PendingFile[]; height: number }>();

/** Memoized message list — skips re-render when only inputText changes */
interface MessageListProps {
  readingMode?: boolean;
  readOnly?: boolean;
  conversationId?: string | null;
  messages: any[];
  loading: boolean;
  expandedMessages: Set<number>;
  editingMessageIdx: number | null;
  editingContent: string;
  compactStatus: { state: string; message?: string };
  onSetEditingContent?: (v: string) => void;
  onEditCancel?: () => void;
  onEditSave?: () => void;
  onToggleExpand: (idx: number) => void;
  onResend?: (content: UserContent, idx: number) => void;
  onEdit?: (content: UserContent, idx: number) => void;
  onBranch?: (idx: number) => void | Promise<void>;
  onOpenDocument?: (doc: DocumentInfo) => void;
  onSetMessages?: (messages: any[] | ((prev: any[]) => any[])) => void;
  messageContentRefs: React.MutableRefObject<Map<number, HTMLDivElement>>;
  onOpenResearch?: (msgId: string) => void;
  onInspect?: (id: string) => void;
  t: (key: string, params?: Record<string, string | number>) => string;
}

const MessageList = React.memo<MessageListProps>(({
  readingMode = false, readOnly = false, conversationId, messages, loading, expandedMessages, editingMessageIdx, editingContent,
  compactStatus, onSetEditingContent, onEditCancel, onEditSave,
  onToggleExpand, onResend, onEdit, onBranch, onOpenDocument, onSetMessages,
  messageContentRefs, onOpenResearch, onInspect, t,
}) => {
  const showCodingView = useGeneralSettingsStore(state => state.showCodingView);
  const visibleMessages = messages.map((msg, idx) => ({ msg, idx })).filter(({ msg, idx }) => {
    if (!readingMode) return true;
    // The running answer has no text yet; keep it so its reply indicator stays visible.
    if (loading && idx === messages.length - 1 && msg.role === 'assistant') return true;
    if (msg.is_summary === 1 || msg.is_compact_boundary || !['user', 'assistant'].includes(msg.role)) return false;
    return msg.role === 'user' || messageSteps(msg).some(step => step.kind === 'text' && step.text?.trim()) || normalizeMessageDocuments(msg).length > 0 || msg.codeImages?.length > 0;
  });
  return (
    <div className="conversation-content" onClickCapture={event => {
      const anchor = (event.target as HTMLElement).closest('a');
      if (!anchor || !/^https?:\/\//i.test(anchor.href)) return;
      event.preventDefault();
      event.stopPropagation();
      void openChatLink(anchor.href).catch(error => console.error('Failed to open chat link:', error));
    }}>
      <style>{`
        @keyframes shimmer {
          0% { background-position: 200% 0; }
          100% { background-position: -200% 0; }
        }
        .animate-shimmer-text {
          background: linear-gradient(90deg, var(--text-claude-secondary) 45%, var(--text-claude-main) 50%, var(--text-claude-secondary) 55%);
          background-size: 200% 100%;
          -webkit-background-clip: text;
          -webkit-text-fill-color: transparent;
          background-clip: text;
          animation: shimmer 4s linear infinite;
        }
      `}</style>
      {visibleMessages.map(({ msg, idx }, visibleIndex) => {
        const readingDivider = readingMode && visibleIndex > 0 && visibleMessages[visibleIndex - 1].msg.role !== msg.role;
        const isCurrentlyStreaming = loading && idx === messages.length - 1;
        const timingLabel = msg.role === 'assistant' && (msg.turn?.status !== 'running' || loading) && !messages.slice(0, idx).some(previous => previous.role === 'assistant' && previous.turn?.turnId === msg.turn?.turnId) ? turnLabel(msg.turn) : null;
        const isPairStart = msg.role === 'user' && (idx === 0 || messages[idx - 1]?.role === 'assistant');
        return (
        <div key={msg.id || idx} data-message-index={idx} className="group" style={{ marginBottom: isPairStart && idx > 0 ? '28px' : 'var(--msg-gap)' }}>
          {readingDivider && <hr className="mb-5 border-0 border-t border-claude-border" />}
          {(msg.is_summary === 1 || msg.is_compact_boundary) && (
            <div className="flex items-center gap-3 mb-5 mt-2">
              <div className="flex-1 h-px bg-claude-border" />
              <span className="text-[12px] text-claude-textSecondary whitespace-nowrap">Context compacted above this point</span>
              <div className="flex-1 h-px bg-claude-border" />
            </div>
          )}
          {(msg.is_summary === 1 || msg.is_compact_boundary) ? null : msg.role === 'user' ? (
            !readingMode && !readOnly && editingMessageIdx === idx ? (
              <div className="w-full bg-[#F0EEE7] dark:bg-claude-btnHover rounded-xl p-3 border border-black/5 dark:border-white/10">
                <div className="bg-white dark:bg-black/20 rounded-lg border border-black/10 dark:border-white/10 focus-within:ring-2 focus-within:ring-blue-500/20 focus-within:border-blue-500 transition-all p-3">
                  <textarea
                    className="w-full bg-transparent text-claude-text outline-none resize-none text-[16px] leading-relaxed font-sans font-[350] block"
                    value={editingContent}
                    onChange={(e) => {
                      onSetEditingContent?.(e.target.value);
                      e.target.style.height = 'auto';
                      e.target.style.height = e.target.scrollHeight + 'px';
                    }}
                    onKeyDown={(e) => { if (e.key === 'Escape') onEditCancel?.(); }}
                    ref={(el) => {
                      if (el) {
                        el.style.height = 'auto';
                        el.style.height = el.scrollHeight + 'px';
                        el.focus();
                      }
                    }}
                    style={{ minHeight: '60px' }}
                  />
                </div>
                <div className="flex items-start justify-between mt-3 px-1 gap-4">
                  <div className="flex items-start gap-2 text-claude-textSecondary text-[13px] leading-tight pt-1">
                    <Info size={14} className="mt-0.5 shrink-0" />
                    <span>
                      {t('chat.editingMessageWarning')}
                    </span>
                  </div>
                  <div className="flex items-center gap-2 shrink-0">
                    <button
                      onClick={onEditCancel}
                      className="px-3 py-1.5 text-[13px] font-medium text-claude-text bg-white dark:bg-claude-bg border border-black/10 dark:border-white/10 hover:bg-gray-50 dark:hover:bg-claude-hover rounded-lg transition-colors"
                    >
                      {t('chat.cancelEdit')}
                    </button>
                    <button
                      onClick={onEditSave}
                      disabled={!editingContent.trim() || editingContent === extractTextContent(msg.content)}
                      className="px-3 py-1.5 text-[13px] font-medium text-white bg-claude-text hover:bg-claude-textSecondary rounded-lg transition-colors disabled:opacity-30 disabled:cursor-not-allowed"
                    >
                      {t('chat.saveEdit')}
                    </button>
                  </div>
                </div>
              </div>
            ) : (
              <div className="flex flex-col items-end gap-1.5">
                {msg.attachments && msg.attachments.length > 0 && (
                  <div className="max-w-[85%] w-fit">
                    <MessageAttachments attachments={msg.attachments} onOpenDocument={onOpenDocument} />
                  </div>
                )}
                {(!msg.attachments || msg.attachments.length === 0) && msg.has_attachments === 1 && (
                  <div className="max-w-[85%] w-fit">
                    <div className="bg-[#F0EEE7] dark:bg-claude-input/60 text-claude-textSecondary px-4 py-2 text-[14px] rounded-xl font-sans italic border border-black/5 dark:border-white/5">
                      📎 Files attached
                    </div>
                  </div>
                )}
                {contentBlocks(msg.content) ? <UserContentBlocks content={msg.content} /> : !!extractTextContent(msg.content).trim() && (
                  <div className="max-w-[85%] w-fit">
                    <div
                      className="conversation-text bg-[var(--msg-user-bg)] text-claude-text px-4 py-[10px] text-[15px] leading-relaxed font-sans font-[400] whitespace-pre-wrap break-words relative overflow-hidden"
                      style={{ borderRadius: 'var(--msg-user-radius)', maxHeight: expandedMessages.has(idx) ? 'none' : '300px' }}
                      ref={(el) => { if (el) messageContentRefs.current.set(idx, el); }}
                    >
                      {(() => {
                        try {
                          const text = extractTextContent(msg.content);
                          if (!text) return '';
                          const skillMatch = text.match(/^\/([a-zA-Z0-9_-]+)(\s|$)/);
                          if (skillMatch) {
                            const slug = skillMatch[1];
                            const rest = text.slice(skillMatch[0].length);
                            return <>
                              <span className="text-[#4B9EFA] font-medium">/{slug}</span>
                              {rest ? ' ' + rest : ''}
                            </>;
                          }
                          return text;
                        } catch { return extractTextContent(msg.content) || ''; }
                      })()}
                      {!expandedMessages.has(idx) && (() => {
                        const el = messageContentRefs.current.get(idx);
                        return el && el.scrollHeight > 300;
                      })() && (
                          <div className="absolute bottom-0 left-0 right-0 h-16 bg-gradient-to-t from-[#F0EEE7] dark:from-claude-btnHover to-transparent pointer-events-none" />
                        )}
                    </div>
                    {(() => {
                      const el = messageContentRefs.current.get(idx);
                      const isOverflow = el && el.scrollHeight > 300;
                      if (!isOverflow) return null;
                      return (
                        <div className="bg-[#F0EEE7] dark:bg-claude-btnHover rounded-b-2xl px-3.5 pb-3 pt-1 -mt-[1px] relative" style={{ borderTopLeftRadius: 0, borderTopRightRadius: 0 }}>
                          <button onClick={() => onToggleExpand(idx)} className="text-[13px] text-claude-textSecondary hover:text-claude-text transition-colors">
                            {expandedMessages.has(idx) ? 'Show less' : 'Show more'}
                          </button>
                        </div>
                      );
                    })()}
                  </div>
                )}
                {!contentBlocks(msg.content) && hasUnavailableImages(msg.content) && <div role="note" className="text-sm text-claude-textSecondary">Image unavailable: this historical message contains only a placeholder.</div>}
                {!readingMode && <div className="flex items-center gap-1.5 mt-1.5 pr-1">
                  {msg.created_at && (
                    <span className="text-[12px] tabular-nums text-claude-textSecondary mr-1">{formatMessageTime(msg.created_at)}</span>
                  )}
                  <div className="message-actions flex items-center gap-0.5 opacity-0 transition-opacity duration-150 group-hover:opacity-100 group-focus-within:opacity-100">
                    {!readOnly && <button onClick={() => onResend?.(msg.content, idx)} className="p-1 text-claude-textSecondary hover:text-claude-text hover:bg-claude-hover rounded transition-colors" title="重新发送"><RotateCcw size={14} /></button>}
                    {!readOnly && <button onClick={() => onEdit?.(msg.content, idx)} className="p-1 text-claude-textSecondary hover:text-claude-text hover:bg-claude-hover rounded transition-colors" title="编辑"><Pencil size={14} /></button>}
                    <CopyButton key={`${conversationId}:${idx}`} text={extractTextContent(msg.content)} />
                  </div>
                </div>}
              </div>
            )
          ) : (
            <div className="text-claude-text text-[16.5px] leading-normal mt-2">
              {!readingMode && timingLabel && <div className="mb-4 text-sm tabular-nums text-claude-textSecondary" aria-label="Turn duration">{timingLabel}<div className="mt-3 h-px w-full bg-claude-border" /></div>}
              {/* Research badge */}
              {!readingMode && msg.research && (
                <button
                  onClick={() => onOpenResearch && onOpenResearch(msg.id)}
                  className="mb-3 inline-flex items-center gap-2 px-3 py-2 rounded-lg bg-[#DBEAFE] dark:bg-[#1E3A5F] hover:bg-[#BFDBFE] dark:hover:bg-[#2A4A75] transition-colors"
                >
                  {msg.research.completed ? (
                    <IconResearch size={16} className="text-[#2E7CF6]" />
                  ) : (
                    <Loader2 size={16} className="text-[#2E7CF6] animate-spin" />
                  )}
                  <div className="text-left">
                    <div className="text-[12.5px] font-medium text-[#2E7CF6] leading-tight">
                      {msg.research.completed
                        ? `Research complete · ${(msg.research.sources || []).length} sources`
                        : msg.research.phase_label || 'Researching...'}
                    </div>
                    {msg.research.plan?.title && (
                      <div className="text-[11px] text-[#2E7CF6]/70 leading-tight mt-0.5 truncate max-w-[400px]">
                        {msg.research.plan.title}
                      </div>
                    )}
                  </div>
                </button>
              )}
              <WorkDetails textOnly={readingMode} conversationId={conversationId} message={msg} index={idx} live={isCurrentlyStreaming} onInspect={onInspect} />
              {!readingMode && msg.searchStatus && (!msg.searchLogs || msg.searchLogs.length === 0) && (!msg.content || msg.content.length === (msg._contentLenBeforeSearch || 0)) && loading && idx === messages.length - 1 && (
                <div className="flex items-center justify-center gap-2 text-[15px] font-medium mb-4 w-full">
                  <Globe size={18} className="text-claude-textSecondary" />
                  <span className="animate-shimmer-text">
                    Searching the web
                  </span>
                </div>
              )}

              {!readingMode && msg.searchLogs && msg.searchLogs.length > 0 && (
                <SearchProcess logs={msg.searchLogs} isThinking={msg.isThinking} isDone={(msg.content || '').length > (msg._contentLenBeforeSearch || 0)} />
              )}

              {!readingMode && normalizeDocumentDrafts(msg).length > 0 && (
                <DocumentCreationProcess drafts={normalizeDocumentDrafts(msg)} />
              )}

              {!readingMode && showCodingView && (msg as any).codeDiffs && (msg as any).codeDiffs.length > 0 && (
                <div className="mt-2 mb-1 space-y-2">
                  {(msg as any).codeDiffs.map((diff: any) => (
                    <DiffViewer
                      key={diff.id}
                      filePath={diff.file_path}
                      originalContent={diff.original_content}
                      modifiedContent={diff.modified_content}
                      diffText={diff.diff_text}
                      status={diff.status || 'pending'}
                      onApply={readOnly ? undefined : async () => {
                        try {
                          await invoke('apply_diff', { diffId: diff.id });
                          if ((msg as any).codeDiffs) {
                            const updated = (msg as any).codeDiffs.map((d: any) =>
                              d.id === diff.id ? { ...d, status: 'applied', applied_at: new Date().toISOString() } : d
                            );
                            onSetMessages?.((prev: any) =>
                              prev.map((m: any, i: number) => {
                                if (i !== idx) return m;
                                return { ...m, codeDiffs: updated };
                              })
                            );
                          }
                        } catch (e) {
                          console.error('Failed to apply diff:', e);
                        }
                      }}
                      onReject={readOnly ? undefined : async () => {
                        try {
                          await invoke('reject_diff', { diffId: diff.id });
                          if ((msg as any).codeDiffs) {
                            const updated = (msg as any).codeDiffs.map((d: any) =>
                              d.id === diff.id ? { ...d, status: 'rejected' } : d
                            );
                            onSetMessages?.((prev: any) =>
                              prev.map((m: any, i: number) => {
                                if (i !== idx) return m;
                                return { ...m, codeDiffs: updated };
                              })
                            );
                          }
                        } catch (e) {
                          console.error('Failed to reject diff:', e);
                        }
                      }}
                    />
                  ))}
                </div>
              )}
              {normalizeMessageDocuments(msg).length > 0 && (
                <div className="mt-2 mb-1 space-y-2">
                  {normalizeMessageDocuments(msg).map((doc, docIdx) => (
                    <DocumentCard
                      key={doc.id || `${idx}-${docIdx}`}
                      document={doc}
                      onOpen={(openedDoc) => onOpenDocument?.(openedDoc)}
                    />
                  ))}
                </div>
              )}
              {!readingMode && msg.codeExecution && (
                <CodeExecution
                  code={msg.codeExecution.code}
                  status={msg.codeExecution.status}
                  stdout={msg.codeExecution.stdout}
                  stderr={msg.codeExecution.stderr}
                  images={msg.codeExecution.images}
                  error={msg.codeExecution.error}
                />
              )}
              {!msg.codeExecution && (msg as any).codeImages && (msg as any).codeImages.length > 0 && (
                <div className="my-3 space-y-2">
                  {(msg as any).codeImages.map((url: string, i: number) => (
                    <div key={i} className="rounded-lg overflow-hidden">
                      <img src={withAuthToken(url)} alt={`图表 ${i + 1}`} className="max-w-full" />
                    </div>
                  ))}
                </div>
              )}
              {loading && idx === messages.length - 1 && !msg.content && !msg.searchStatus && normalizeDocumentDrafts(msg).length === 0 && !(msg.toolCalls && msg.toolCalls.length > 0) && (
                <div role="status" className="flex items-center gap-2 ml-1 text-[14px] text-claude-textSecondary font-sans">
                  <span aria-hidden="true" className="shrink-0"><ClaudeLogo breathe style={{ width: '40px', height: '40px' }} /></span>
                  <span>Working on it...</span>
                </div>
              )}
              {loading && idx === messages.length - 1 && !msg.isThinking && (msg.content || (msg.searchStatus && msg.content)) && (
                <span className="inline-block ml-1 align-middle" style={{ verticalAlign: 'middle' }}>
                  <ClaudeLogo autoAnimate style={{ width: '40px', height: '40px', display: 'inline-block' }} />
                </span>
              )}
              {!readingMode && !isCurrentlyStreaming && Boolean(msg.content || msg.thinking || msg.toolCalls?.length) && (
                <ResponseFooter key={`${conversationId || 'unsaved'}:${msg.id || idx}`} message={msg} index={idx} conversationId={conversationId} disabled={loading} readOnly={readOnly} onBranch={onBranch} />
              )}
              {!readingMode && !loading && idx === messages.length - 1 && msg.content && compactStatus.state === 'compacting' && (
                <div className="flex items-start gap-4 mt-6 ml-1 mb-2">
                  <ClaudeLogo breathe={compactStatus.state === 'compacting'} style={{ width: '36px', height: '36px', flexShrink: 0, marginTop: '2px' }} />
                  {compactStatus.state === 'compacting' && <CompactingStatus />}
                </div>
              )}
            </div>
          )}
        </div>
        );
      })}
    </div>
  );
});

export default MessageList;
