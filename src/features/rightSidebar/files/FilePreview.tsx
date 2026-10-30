import { useEffect, useState } from 'react';
import { Prism as SyntaxHighlighter } from 'react-syntax-highlighter';
import { oneLight, vscDarkPlus } from 'react-syntax-highlighter/dist/esm/styles/prism';
import { AlertCircle, ArrowLeft, Check, Copy, FileWarning, Loader2 } from 'lucide-react';
import { readFileContent, type FsFileNode } from '../../../api';
import { useI18n } from '../../../hooks/useI18n';
import { copyToClipboard } from '../../../utils/clipboard';
import { MAX_HIGHLIGHT_CHARS, MAX_PREVIEW_BYTES, formatBytes, languageForFile, looksBinary, relativePath } from './fileUtils';

type PreviewState =
  | { status: 'loading' }
  | { status: 'text'; content: string }
  | { status: 'tooLarge'; size: number }
  | { status: 'binary'; size: number }
  | { status: 'error'; message: string };

const MONO_FONT = 'Menlo, Monaco, "SF Mono", "Cascadia Code", "Fira Code", Consolas, "Courier New", monospace';

function useIsDark() {
  const [isDark, setIsDark] = useState(() => document.documentElement.classList.contains('dark'));
  useEffect(() => {
    const observer = new MutationObserver(() => setIsDark(document.documentElement.classList.contains('dark')));
    observer.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
    return () => observer.disconnect();
  }, []);
  return isDark;
}

/** Read-only preview of one workspace file. `reloadKey` changes re-fetch the file. */
export default function FilePreview({ root, node, reloadKey, onBack }: {
  root: string;
  node: FsFileNode;
  reloadKey: number;
  onBack: () => void;
}) {
  const { t, locale } = useI18n();
  const isDark = useIsDark();
  const [state, setState] = useState<PreviewState>({ status: 'loading' });
  const [retry, setRetry] = useState(0);
  const [copied, setCopied] = useState(false);
  const relative = relativePath(root, node.path);

  useEffect(() => {
    if (node.size > MAX_PREVIEW_BYTES) {
      setState({ status: 'tooLarge', size: node.size });
      return;
    }
    let cancelled = false;
    setState(prev => (prev.status === 'text' ? prev : { status: 'loading' }));
    readFileContent(node.path, { maxBytes: MAX_PREVIEW_BYTES })
      .then(res => {
        if (cancelled) return;
        const size = typeof res.size === 'number' ? res.size : node.size;
        const content = typeof res.content === 'string' ? res.content : '';
        if (res.binary || looksBinary(content)) setState({ status: 'binary', size });
        else if (res.truncated || size > MAX_PREVIEW_BYTES) setState({ status: 'tooLarge', size });
        else setState({ status: 'text', content });
      })
      .catch((error: unknown) => {
        if (!cancelled) setState({ status: 'error', message: error instanceof Error ? error.message : String(error) });
      });
    return () => { cancelled = true; };
  }, [node.path, node.size, reloadKey, retry]);

  useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 2000);
    return () => window.clearTimeout(timer);
  }, [copied]);

  const handleCopy = async () => {
    if (await copyToClipboard(node.path)) setCopied(true);
  };

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-9 flex-shrink-0 items-center gap-1 border-b border-claude-border px-2">
        <button type="button" onClick={onBack} aria-label={t('rightSidebar.filesBack')} title={t('rightSidebar.filesBack')}
          className="rounded-md p-1 text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text">
          <ArrowLeft size={15} aria-hidden="true" />
        </button>
        <span className="min-w-0 flex-1 truncate font-mono text-[12px] text-claude-text" title={node.path} data-testid="file-preview-path">{relative}</span>
        <button type="button" onClick={() => { void handleCopy(); }}
          aria-label={copied ? t('rightSidebar.filesPathCopied') : t('rightSidebar.filesCopyPath')}
          title={copied ? t('rightSidebar.filesPathCopied') : t('rightSidebar.filesCopyPath')}
          className="rounded-md p-1 text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text">
          {copied ? <Check size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />}
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-auto bg-[#FAFAFA] dark:bg-[#1E1E1E]" role="region" aria-label={relative}>
        {state.status === 'loading' && (
          <div className="flex items-center gap-2 p-4 text-[13px] text-claude-textSecondary" role="status">
            <Loader2 size={14} className="animate-spin" aria-hidden="true" />{t('rightSidebar.filesLoadingFile')}
          </div>
        )}
        {(state.status === 'tooLarge' || state.status === 'binary') && (
          <div className="flex flex-col items-center gap-2 px-6 py-10 text-center text-[13px] text-claude-textSecondary">
            <FileWarning size={20} strokeWidth={1.5} aria-hidden="true" />
            <p className="font-medium text-claude-text">{state.status === 'tooLarge' ? t('rightSidebar.filesTooLarge') : t('rightSidebar.filesBinary')}</p>
            <p>{formatBytes(state.size, locale)}</p>
          </div>
        )}
        {state.status === 'error' && (
          <div className="flex flex-col items-center gap-2 px-6 py-10 text-center text-[13px]" role="alert">
            <AlertCircle size={20} strokeWidth={1.5} className="text-red-600 dark:text-red-400" aria-hidden="true" />
            <p className="font-medium text-claude-text">{t('rightSidebar.filesReadError')}</p>
            <p className="break-all text-claude-textSecondary">{state.message}</p>
            <button type="button" onClick={() => setRetry(n => n + 1)} className="mt-1 rounded-md border border-claude-border px-3 py-1 text-claude-text hover:bg-claude-hover">
              {t('rightSidebar.filesRetry')}
            </button>
          </div>
        )}
        {state.status === 'text' && <FileContent content={state.content} language={languageForFile(node.name)} isDark={isDark} />}
      </div>
    </div>
  );
}

function FileContent({ content, language, isDark }: { content: string; language: string | null; isDark: boolean }) {
  const lineNumberStyle = { minWidth: '2.5em', paddingRight: '12px', textAlign: 'right' as const, opacity: 0.45, userSelect: 'none' as const };
  if (language && content.length <= MAX_HIGHLIGHT_CHARS) {
    return (
      <SyntaxHighlighter
        language={language}
        style={isDark ? vscDarkPlus : oneLight}
        showLineNumbers
        lineNumberStyle={lineNumberStyle}
        customStyle={{ margin: 0, padding: '8px 12px 8px 4px', background: 'transparent', fontSize: '12px', lineHeight: '1.6', fontFamily: MONO_FONT, overflow: 'visible' }}
        codeTagProps={{ style: { fontFamily: 'inherit' } }}
      >
        {content}
      </SyntaxHighlighter>
    );
  }
  // Plain text: two aligned columns instead of one element per line, so large files stay cheap.
  const lineCount = content.split('\n').length;
  const numbers = Array.from({ length: lineCount }, (_, i) => i + 1).join('\n');
  return (
    <div className="flex min-w-max text-[12px] leading-[1.6] text-claude-text" style={{ fontFamily: MONO_FONT }}>
      <pre aria-hidden="true" className="m-0 select-none py-2 pl-1 pr-3 text-right" style={{ minWidth: '2.5em', opacity: 0.45 }}>{numbers}</pre>
      <pre className="m-0 py-2 pr-3" data-testid="file-preview-text">{content}</pre>
    </div>
  );
}
