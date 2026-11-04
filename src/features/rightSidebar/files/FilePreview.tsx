import { useEffect, useRef, useState } from 'react';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { oneLight, vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { getWorkspaceFileDiff, readWorkspaceFile, type WorkspaceFile, type WorkspaceFileDiff } from '../../../api/workspaceFiles';
import type { SidebarFileTab } from '../../../stores/useRightSidebarStore';
import { MAX_HIGHLIGHT_CHARS, languageForFile } from './fileUtils';

const MONO_FONT = 'Menlo, Monaco, "SF Mono", "Cascadia Code", "Fira Code", Consolas, "Courier New", monospace';
function useIsDark() {
  const [dark, setDark] = useState(() => document.documentElement.classList.contains('dark'));
  useEffect(() => {
    const observer = new MutationObserver(() => setDark(document.documentElement.classList.contains('dark')));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
    return () => observer.disconnect();
  }, []);
  return dark;
}

export default function FilePreview({ file, diff, active, reloadKey }: {
  file: SidebarFileTab; diff: boolean; active: boolean; reloadKey: number;
}) {
  const [data, setData] = useState<WorkspaceFile | null>(null);
  const [patch, setPatch] = useState<WorkspaceFileDiff | null>(null);
  const [error, setError] = useState('');
  const [retry, setRetry] = useState(0);
  const container = useRef<HTMLDivElement>(null);
  const dark = useIsDark();
  useEffect(() => {
    const controller = new AbortController();
    setError('');
    if (diff) {
      setPatch(null);
      getWorkspaceFileDiff(file.scope, file, controller.signal).then(value => { if (!controller.signal.aborted) setPatch(value); }).catch(fail);
    } else {
      setData(null);
      readWorkspaceFile(file.scope, file, controller.signal).then(value => { if (!controller.signal.aborted) setData(value); }).catch(fail);
    }
    function fail(error: unknown) { if (!controller.signal.aborted) setError(error instanceof Error ? error.message : String(error)); }
    return () => controller.abort();
  }, [file.id, diff, reloadKey, retry]);
  useEffect(() => {
    if (!active || diff || !file.position || !data) return;
    const match = container.current?.querySelector('[data-search-match]');
    match?.scrollIntoView?.({ block: 'center', inline: 'center' });
  }, [active, diff, data, file.navigation]);
  return <div ref={container} role="region" aria-label={file.relativePath} className="h-full min-h-0 overflow-auto bg-claude-bg">
    {error ? <div role="alert" className="p-4 text-sm text-red-600">{error}<button type="button" className="ml-2 underline" onClick={() => setRetry(value => value + 1)}>Retry</button></div> : diff ? patch ? <>
      <p className="sticky top-0 z-10 bg-claude-bg px-3 py-2 text-xs text-claude-textSecondary">
        {patch.status === 'not-git' ? 'No Git baseline' : patch.status === 'untracked' ? 'New file vs empty content' : patch.baseline === 'head' ? `Working tree vs HEAD · ${patch.baselineOid?.slice(0, 8)}` : 'Working tree vs empty tree'}
      </p>
      {patch.notice && <p className="px-3 py-2 text-xs text-claude-textSecondary">{patch.notice}</p>}
      {patch.status === 'unchanged' && <p className="p-4 text-sm text-claude-textSecondary">No differences.</p>}
      {patch.patch && <pre className="min-w-max p-3 text-xs leading-relaxed">{patch.patch.split('\n').map((line, index) => <div key={index} className={line.startsWith('+') ? 'bg-green-500/10 text-green-700 dark:text-green-300' : line.startsWith('-') ? 'bg-red-500/10 text-red-700 dark:text-red-300' : 'text-claude-text'}>{line || ' '}</div>)}</pre>}
    </> : <p role="status" className="p-4 text-sm">Loading diff…</p> : data ? <>
      {data.notice && <p className="p-3 text-xs text-claude-textSecondary">{data.notice}</p>}
      {data.binary ? <p className="p-4 text-sm text-claude-textSecondary">Binary file · {data.size} bytes</p> : data.content !== null ? <FileContent content={data.content} file={file} dark={dark} /> : <p className="p-4 text-sm">File content is unavailable.</p>}
    </> : <p role="status" className="p-4 text-sm">Loading file…</p>}
  </div>;
}

function FileContent({ content, file, dark }: { content: string; file: SidebarFileTab; dark: boolean }) {
  const language = languageForFile(file.name);
  const position = file.position;
  if (position) {
    const lines = content.split('\n');
    const index = position.line - 1;
    if (index >= 0 && index < lines.length) {
      const line = lines[index];
      return <pre className="min-w-max p-3 text-xs leading-[1.6] text-claude-text" style={{ fontFamily: MONO_FONT }}>
        {lines.slice(0, index).join('\n')}{index > 0 ? '\n' : ''}
        {line.slice(0, position.startColumn - 1)}<mark data-search-match tabIndex={-1} className="bg-yellow-300/50 text-inherit">{line.slice(position.startColumn - 1, position.endColumn - 1)}</mark>{line.slice(position.endColumn - 1)}
        {index < lines.length - 1 ? '\n' : ''}{lines.slice(index + 1).join('\n')}
      </pre>;
    }
  }
  if (language && content.length <= MAX_HIGHLIGHT_CHARS) return <SyntaxHighlighter
    language={language} style={dark ? vscDarkPlus : oneLight} showLineNumbers
    lineNumberStyle={{ minWidth: '2.5em', paddingRight: '12px', textAlign: 'right', opacity: 0.45, userSelect: 'none' }}
    customStyle={{ margin: 0, padding: '8px 12px 8px 4px', background: 'transparent', fontSize: '12px', lineHeight: '1.6', fontFamily: MONO_FONT, overflow: 'visible' }}
    codeTagProps={{ style: { fontFamily: 'inherit' } }}
  >{content}</SyntaxHighlighter>;
  const numbers = Array.from({ length: content.split('\n').length }, (_, i) => i + 1).join('\n');
  return <div className="flex min-w-max text-[12px] leading-[1.6] text-claude-text" style={{ fontFamily: MONO_FONT }}>
    <pre aria-hidden="true" className="m-0 select-none py-2 pl-1 pr-3 text-right opacity-45">{numbers}</pre>
    <pre className="m-0 py-2 pr-3" data-testid="file-preview-text">{content}</pre>
  </div>;
}
