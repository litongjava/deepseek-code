import React, { useCallback, useEffect, useRef, useState } from 'react';
import { ArrowLeft, ArrowRight, ExternalLink, Info, RefreshCw, X, ZoomIn, ZoomOut } from 'lucide-react';
import { useI18n } from '../../../hooks/useI18n';
import AddressBar from './AddressBar';
import { isLoopbackUrl } from './browserUrl';
import { openInSystemBrowser } from './openInSystemBrowser';

const ZOOM_STEP = 0.25;
const MIN_ZOOM = 0.25;
const MAX_ZOOM = 2;
/** Without a load event by then, the page may be blocked from embedding. */
export const LOAD_HINT_DELAY_MS = 4000;

/**
 * allow-same-origin keeps a framed dev server on its own origin (cookies, storage,
 * HMR sockets). Together with allow-scripts it would let a page *same-origin with
 * this app* remove its sandbox, so normalizeBrowserUrl refuses the app's origin.
 * Popups may leave the sandbox so OAuth and "open in new tab" links keep working.
 */
const FRAME_SANDBOX = 'allow-scripts allow-same-origin allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals allow-downloads';

interface Navigation { url: string; id: number }

const iconButton = 'flex-shrink-0 rounded-md p-1 text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text disabled:cursor-not-allowed disabled:opacity-30 disabled:hover:bg-transparent';

/**
 * Iframe browser for the right sidebar. Back and Forward walk the pages opened
 * through this component (address bar, chat links); navigation inside a
 * cross-origin page cannot be observed, so it is not part of that history.
 */
export default function EmbeddedBrowser({ initialUrl, navigation, onUrlChange, onClose }: {
  initialUrl: string;
  /** A new id navigates to `url`, pushing it onto the history. */
  navigation?: Navigation;
  /** Called with the page shown after every navigation, for persistence. */
  onUrlChange?: (url: string) => void;
  /** Leave the page and return to the start view. */
  onClose?: () => void;
}) {
  const { t } = useI18n();
  const [history, setHistory] = useState<string[]>([initialUrl]);
  const [index, setIndex] = useState(0);
  const [frameKey, setFrameKey] = useState(0);
  const [zoom, setZoom] = useState(1);
  const [loaded, setLoaded] = useState(false);
  const [timedOut, setTimedOut] = useState(false);
  const [hintDismissed, setHintDismissed] = useState(false);
  const currentUrl = history[index];
  const lastNavigationId = useRef(navigation?.id);

  const navigate = useCallback((url: string) => {
    setHistory(prev => [...prev.slice(0, index + 1), url]);
    setIndex(index + 1);
    setFrameKey(key => key + 1);
  }, [index]);

  useEffect(() => {
    if (!navigation || navigation.id === lastNavigationId.current) return;
    lastNavigationId.current = navigation.id;
    navigate(navigation.url);
  }, [navigation, navigate]);

  const onUrlChangeRef = useRef(onUrlChange);
  onUrlChangeRef.current = onUrlChange;
  useEffect(() => { onUrlChangeRef.current?.(currentUrl); }, [currentUrl]);

  // Each navigation remounts the frame, so load state and the hint timer start over.
  useEffect(() => {
    setLoaded(false);
    setTimedOut(false);
    setHintDismissed(false);
    const timer = window.setTimeout(() => setTimedOut(true), LOAD_HINT_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [frameKey]);

  const go = (offset: -1 | 1) => {
    const next = index + offset;
    if (next < 0 || next >= history.length) return;
    setIndex(next);
    setFrameKey(key => key + 1);
  };

  const isBlank = currentUrl === 'about:blank';
  const loading = !loaded && !timedOut && !isBlank;
  // Embedding failures are invisible to a cross-origin parent: hint after a missing
  // load event, and always for remote hosts, which often forbid framing.
  const showHint = !isBlank && !hintDismissed && ((timedOut && !loaded) || ((loaded || timedOut) && !isLoopbackUrl(currentUrl)));
  const zoomPercent = Math.round(zoom * 100);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex-shrink-0 space-y-1 border-b border-claude-border px-2 py-1.5">
        <div className="flex items-start gap-0.5">
          <button type="button" className={iconButton} onClick={() => go(-1)} disabled={index <= 0} aria-label={t('rightSidebar.browserBack')} title={t('rightSidebar.browserBack')}>
            <ArrowLeft size={15} aria-hidden="true" />
          </button>
          <button type="button" className={iconButton} onClick={() => go(1)} disabled={index >= history.length - 1} aria-label={t('rightSidebar.browserForward')} title={t('rightSidebar.browserForward')}>
            <ArrowRight size={15} aria-hidden="true" />
          </button>
          <button type="button" className={iconButton} onClick={() => setFrameKey(key => key + 1)} aria-label={t('rightSidebar.browserReload')} title={t('rightSidebar.browserReload')}>
            <RefreshCw size={15} className={loading ? 'animate-spin' : ''} aria-hidden="true" />
          </button>
          <AddressBar value={currentUrl} onNavigate={navigate} className="ml-1 flex-1" />
        </div>
        <div className="flex items-center gap-0.5">
          <button type="button" className={iconButton} onClick={() => setZoom(z => Math.max(MIN_ZOOM, z - ZOOM_STEP))} disabled={zoom <= MIN_ZOOM} aria-label={t('rightSidebar.browserZoomOut')} title={t('rightSidebar.browserZoomOut')}>
            <ZoomOut size={14} aria-hidden="true" />
          </button>
          <button
            type="button"
            className="min-w-[44px] rounded-md px-1 py-0.5 text-center text-[12px] tabular-nums text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text"
            onClick={() => setZoom(1)}
            aria-label={t('rightSidebar.browserZoomReset', { percent: zoomPercent })}
            title={t('rightSidebar.browserZoomReset', { percent: zoomPercent })}
          >
            {zoomPercent}%
          </button>
          <button type="button" className={iconButton} onClick={() => setZoom(z => Math.min(MAX_ZOOM, z + ZOOM_STEP))} disabled={zoom >= MAX_ZOOM} aria-label={t('rightSidebar.browserZoomIn')} title={t('rightSidebar.browserZoomIn')}>
            <ZoomIn size={14} aria-hidden="true" />
          </button>
          <div className="flex-1" />
          <button
            type="button"
            className="flex min-w-0 items-center gap-1 rounded-md px-1.5 py-0.5 text-[12px] text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text disabled:cursor-not-allowed disabled:opacity-30"
            onClick={() => { void openInSystemBrowser(currentUrl); }}
            disabled={isBlank}
            title={t('rightSidebar.browserOpenExternal')}
          >
            <ExternalLink size={13} className="flex-shrink-0" aria-hidden="true" />
            <span className="truncate">{t('rightSidebar.browserOpenExternal')}</span>
          </button>
          {onClose && (
            <button type="button" className={iconButton} onClick={onClose} aria-label={t('rightSidebar.browserClosePage')} title={t('rightSidebar.browserClosePage')}>
              <X size={14} aria-hidden="true" />
            </button>
          )}
        </div>
      </div>

      {showHint && (
        <div role="status" className="flex flex-shrink-0 items-start gap-2 border-b border-claude-border bg-claude-hover/60 px-3 py-2 text-[12px] leading-4 text-claude-textSecondary">
          <Info size={14} className="mt-px flex-shrink-0" aria-hidden="true" />
          <p className="min-w-0 flex-1">
            {t('rightSidebar.browserBlockedHint')}{' '}
            <button type="button" className="font-medium text-claude-text underline underline-offset-2 hover:no-underline" onClick={() => { void openInSystemBrowser(currentUrl); }}>
              {t('rightSidebar.browserOpenExternalShort')}
            </button>
          </p>
          <button type="button" className={iconButton} onClick={() => setHintDismissed(true)} aria-label={t('rightSidebar.browserDismissHint')} title={t('rightSidebar.browserDismissHint')}>
            <X size={13} aria-hidden="true" />
          </button>
        </div>
      )}

      <div className="relative min-h-0 flex-1 overflow-hidden bg-white">
        {loading && (
          <div className="absolute inset-x-0 top-0 z-10 h-0.5 overflow-hidden" role="progressbar" aria-label={t('rightSidebar.browserLoading')}>
            <div className="h-full w-1/3 animate-pulse bg-blue-500" />
          </div>
        )}
        <iframe
          key={frameKey}
          src={currentUrl}
          title={t('rightSidebar.browserFrameTitle', { url: currentUrl })}
          sandbox={FRAME_SANDBOX}
          referrerPolicy="no-referrer"
          onLoad={() => setLoaded(true)}
          className="block border-0"
          style={{
            width: `${100 / zoom}%`,
            height: `${100 / zoom}%`,
            transform: `scale(${zoom})`,
            transformOrigin: 'top left',
          }}
        />
      </div>
    </div>
  );
}
