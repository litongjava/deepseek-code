import { useMemo } from 'react';
import { Globe } from 'lucide-react';
import { useI18n } from '../../hooks/useI18n';
import { useRightSidebarStore } from '../../stores/useRightSidebarStore';
import AddressBar from './browser/AddressBar';
import EmbeddedBrowser from './browser/EmbeddedBrowser';
import { LOCAL_DEV_SUGGESTIONS } from './browser/browserUrl';

export default function RightSidebarBrowserTab() {
  const browserUrl = useRightSidebarStore(s => s.browserUrl);
  const browserRequest = useRightSidebarStore(s => s.browserRequest);
  const setBrowserUrl = useRightSidebarStore(s => s.setBrowserUrl);
  const openBrowser = useRightSidebarStore(s => s.openBrowser);
  // Only a new request id navigates; URL changes made inside the browser are just recorded.
  const navigation = useMemo(() => ({ url: browserUrl, id: browserRequest }), [browserRequest]);

  if (!browserUrl) return <BrowserStart onOpen={openBrowser} />;
  return (
    <EmbeddedBrowser
      initialUrl={browserUrl}
      navigation={navigation}
      onUrlChange={setBrowserUrl}
      onClose={() => setBrowserUrl('')}
    />
  );
}

function BrowserStart({ onOpen }: { onOpen: (url: string) => void }) {
  const { t } = useI18n();
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 px-6 text-center">
      <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-claude-hover text-claude-textSecondary">
        <Globe size={20} strokeWidth={1.5} aria-hidden="true" />
      </div>
      <p className="text-[14px] font-medium text-claude-text">{t('rightSidebar.browserEmptyTitle')}</p>
      <p className="max-w-[280px] text-[13px] leading-5 text-claude-textSecondary">{t('rightSidebar.browserEmptyDescription')}</p>
      <AddressBar value="" onNavigate={onOpen} className="w-full max-w-[320px] text-left">
        <button type="submit" className="flex-shrink-0 rounded-md border border-claude-border px-2.5 py-1 text-[13px] text-claude-text transition-colors hover:bg-claude-hover">
          {t('rightSidebar.browserGo')}
        </button>
      </AddressBar>
      <div className="w-full max-w-[320px] text-left">
        <p id="right-sidebar-browser-suggestions" className="mb-1 text-[12px] text-claude-textSecondary">{t('rightSidebar.browserSuggestions')}</p>
        <ul aria-labelledby="right-sidebar-browser-suggestions" className="flex flex-wrap gap-1.5">
          {LOCAL_DEV_SUGGESTIONS.map(url => (
            <li key={url}>
              <button
                type="button"
                onClick={() => onOpen(url)}
                className="rounded-md border border-claude-border px-2 py-0.5 font-mono text-[12px] text-claude-textSecondary transition-colors hover:bg-claude-hover hover:text-claude-text"
              >
                {url.replace(/^http:\/\//, '')}
              </button>
            </li>
          ))}
        </ul>
      </div>
    </div>
  );
}
