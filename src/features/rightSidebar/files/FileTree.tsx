import React, { useEffect, useMemo, useRef, useState } from 'react';
import { ChevronDown, ChevronRight, File, FileCode, FileText, Folder, FolderOpen, Loader2 } from 'lucide-react';
import type { FsFileNode } from '../../../api';
import { useI18n } from '../../../hooks/useI18n';
import type { DirState } from './useFileTree';
import { languageForFile } from './fileUtils';

type Row =
  | { kind: 'node'; key: string; node: FsFileNode; level: number; parent: string | null; posinset: number; setsize: number }
  | { kind: 'status'; key: string; level: number; text: string; error?: boolean };

interface FileTreeProps {
  root: string;
  dirs: Record<string, DirState>;
  expanded: ReadonlySet<string>;
  selectedPath: string | null;
  /** Moves keyboard focus to a row (for example the file just closed); pass a new object per request. */
  focusRequest?: { path: string } | null;
  onToggle: (path: string) => void;
  onSetExpanded: (path: string, open: boolean) => void;
  onOpenFile: (node: FsFileNode) => void;
}

function fileIcon(name: string) {
  const lower = name.toLowerCase();
  if (lower.endsWith('.md') || lower.endsWith('.txt')) return FileText;
  return languageForFile(name) ? FileCode : File;
}

/** Keyboard-accessible tree (WAI-ARIA tree view pattern, flat rows with aria-level). */
export default function FileTree({ root, dirs, expanded, selectedPath, focusRequest, onToggle, onSetExpanded, onOpenFile }: FileTreeProps) {
  const { t } = useI18n();
  const [activePath, setActivePath] = useState<string | null>(null);
  const itemRefs = useRef(new Map<string, HTMLDivElement>());
  const focusOnRender = useRef<string | null>(null);

  const rows = useMemo(() => {
    const out: Row[] = [];
    const walk = (dirPath: string, level: number, parent: string | null) => {
      const state = dirs[dirPath];
      const entries = state && state.status !== 'error' ? state.entries : undefined;
      if (entries) {
        entries.forEach((node, index) => {
          // The Bridge reports symlink-cycle placeholders without a path.
          if (!node.path) return;
          out.push({ kind: 'node', key: node.path, node, level, parent, posinset: index + 1, setsize: entries.length });
          if (node.is_dir && expanded.has(node.path)) walk(node.path, level + 1, node.path);
        });
        if (entries.length === 0 && parent !== null) {
          out.push({ kind: 'status', key: `${dirPath}:empty`, level, text: t('rightSidebar.filesEmptyFolder') });
        }
      } else if (state?.status === 'error' && parent !== null) {
        out.push({ kind: 'status', key: `${dirPath}:error`, level, text: t('rightSidebar.filesFolderError'), error: true });
      }
    };
    walk(root, 1, null);
    return out;
  }, [root, dirs, expanded, t]);

  const nodeRows = useMemo(() => rows.filter((row): row is Extract<Row, { kind: 'node' }> => row.kind === 'node'), [rows]);
  const tabStop = nodeRows.some(row => row.node.path === activePath) ? activePath : (nodeRows[0]?.node.path ?? null);

  const focusItem = (path: string) => {
    setActivePath(path);
    const element = itemRefs.current.get(path);
    if (element) element.focus();
    else focusOnRender.current = path;
  };

  useEffect(() => {
    if (focusRequest) focusItem(focusRequest.path);
  }, [focusRequest]);

  useEffect(() => {
    const path = focusOnRender.current;
    if (path && itemRefs.current.has(path)) {
      focusOnRender.current = null;
      itemRefs.current.get(path)?.focus();
    }
  });

  const activate = (node: FsFileNode) => {
    if (node.is_dir) onToggle(node.path);
    else onOpenFile(node);
  };

  const handleKeyDown = (event: React.KeyboardEvent<HTMLDivElement>) => {
    const index = nodeRows.findIndex(row => row.node.path === tabStop);
    if (index < 0) return;
    const row = nodeRows[index];
    const isOpen = row.node.is_dir && expanded.has(row.node.path);
    let handled = true;
    switch (event.key) {
      case 'ArrowDown':
        if (index < nodeRows.length - 1) focusItem(nodeRows[index + 1].node.path);
        break;
      case 'ArrowUp':
        if (index > 0) focusItem(nodeRows[index - 1].node.path);
        break;
      case 'Home':
        focusItem(nodeRows[0].node.path);
        break;
      case 'End':
        focusItem(nodeRows[nodeRows.length - 1].node.path);
        break;
      case 'ArrowRight':
        if (row.node.is_dir && !isOpen) onSetExpanded(row.node.path, true);
        else if (isOpen && nodeRows[index + 1]?.parent === row.node.path) focusItem(nodeRows[index + 1].node.path);
        break;
      case 'ArrowLeft':
        if (isOpen) onSetExpanded(row.node.path, false);
        else if (row.parent) focusItem(row.parent);
        break;
      case 'Enter':
      case ' ':
        activate(row.node);
        break;
      default:
        handled = false;
    }
    if (handled) event.preventDefault();
  };

  return (
    <div role="tree" aria-label={t('rightSidebar.filesTree')} onKeyDown={handleKeyDown} className="py-1 outline-none">
      {rows.map(row => {
        const indent = { paddingLeft: `${8 + (row.level - 1) * 14}px` };
        if (row.kind === 'status') {
          return (
            <div key={row.key} role="treeitem" aria-level={row.level} aria-disabled="true" style={{ paddingLeft: `${8 + (row.level - 1) * 14 + 20}px` }}
              className={`py-1 pr-2 text-[12px] italic ${row.error ? 'text-red-600 dark:text-red-400' : 'text-claude-textSecondary'}`}>
              {row.text}
            </div>
          );
        }
        const { node } = row;
        const isOpen = node.is_dir && expanded.has(node.path);
        const loading = node.is_dir && isOpen && dirs[node.path]?.status === 'loading';
        const Icon = node.is_dir ? (isOpen ? FolderOpen : Folder) : fileIcon(node.name);
        const Chevron = isOpen ? ChevronDown : ChevronRight;
        return (
          <div
            key={row.key}
            ref={element => { if (element) itemRefs.current.set(node.path, element); else itemRefs.current.delete(node.path); }}
            role="treeitem"
            aria-level={row.level}
            aria-posinset={row.posinset}
            aria-setsize={row.setsize}
            aria-expanded={node.is_dir ? isOpen : undefined}
            aria-selected={node.path === selectedPath}
            aria-busy={loading || undefined}
            tabIndex={node.path === tabStop ? 0 : -1}
            title={node.name}
            onClick={() => { setActivePath(node.path); activate(node); }}
            onFocus={() => setActivePath(node.path)}
            style={indent}
            className={`flex cursor-pointer select-none items-center gap-1 py-[3px] pr-2 text-[13px] text-claude-text outline-none hover:bg-claude-hover focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-blue-500 ${node.path === selectedPath ? 'bg-claude-btn-hover' : ''}`}
          >
            <span className="flex w-4 flex-shrink-0 justify-center text-claude-textSecondary" aria-hidden="true">
              {node.is_dir && (loading ? <Loader2 size={13} className="animate-spin" /> : <Chevron size={13} />)}
            </span>
            <Icon size={15} strokeWidth={1.6} aria-hidden="true" className={`flex-shrink-0 ${node.is_dir ? 'text-[#C6613F]' : 'text-claude-textSecondary'}`} />
            <span className="min-w-0 truncate">{node.name}</span>
          </div>
        );
      })}
    </div>
  );
}
