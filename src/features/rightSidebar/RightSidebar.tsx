import React, { useRef } from 'react';
import { X } from 'lucide-react';
import { useI18n } from '../../hooks/useI18n';
import { RIGHT_SIDEBAR_TABS, type RightSidebarTab, useRightSidebarStore } from '../../stores/useRightSidebarStore';
import RightSidebarFilesTab from './RightSidebarFilesTab';
import RightSidebarBrowserTab from './RightSidebarBrowserTab';

export const RIGHT_SIDEBAR_ID = 'right-sidebar';

const tabPanels: Record<RightSidebarTab, React.ComponentType> = {
  files: RightSidebarFilesTab,
  browser: RightSidebarBrowserTab,
};

export default function RightSidebar({ onClose }: { onClose: () => void }) {
  const { t } = useI18n();
  const tab = useRightSidebarStore(s => s.tab);
  const setTab = useRightSidebarStore(s => s.setTab);
  const tabRefs = useRef<Partial<Record<RightSidebarTab, HTMLButtonElement | null>>>({});
  const Panel = tabPanels[tab];

  const handleTabKeyDown = (event: React.KeyboardEvent<HTMLButtonElement>) => {
    const index = RIGHT_SIDEBAR_TABS.indexOf(tab);
    const last = RIGHT_SIDEBAR_TABS.length - 1;
    const next = event.key === 'ArrowRight' ? (index === last ? 0 : index + 1)
      : event.key === 'ArrowLeft' ? (index === 0 ? last : index - 1)
        : event.key === 'Home' ? 0
          : event.key === 'End' ? last
            : null;
    if (next === null) return;
    event.preventDefault();
    const nextTab = RIGHT_SIDEBAR_TABS[next];
    setTab(nextTab);
    tabRefs.current[nextTab]?.focus();
  };

  return (
    <aside id={RIGHT_SIDEBAR_ID} aria-label={t('rightSidebar.panel')} className="flex h-full w-full min-w-0 flex-col bg-claude-bg">
      <div className="flex h-[54px] flex-shrink-0 items-center justify-between gap-2 px-3">
        <div role="tablist" aria-label={t('rightSidebar.tabs')} className="flex min-w-0 items-center gap-1">
          {RIGHT_SIDEBAR_TABS.map(id => {
            const selected = id === tab;
            return (
              <button
                key={id}
                ref={node => { tabRefs.current[id] = node; }}
                type="button"
                role="tab"
                id={`${RIGHT_SIDEBAR_ID}-tab-${id}`}
                aria-selected={selected}
                aria-controls={`${RIGHT_SIDEBAR_ID}-panel-${id}`}
                tabIndex={selected ? 0 : -1}
                onClick={() => setTab(id)}
                onKeyDown={handleTabKeyDown}
                className={`truncate rounded-md px-2.5 py-1.5 text-[13px] transition-colors outline-none focus-visible:ring-2 focus-visible:ring-blue-500 ${selected ? 'bg-claude-btn-hover font-medium text-claude-text' : 'text-claude-textSecondary hover:bg-claude-hover hover:text-claude-text'}`}
              >
                {t(`rightSidebar.${id}`)}
              </button>
            );
          })}
        </div>
        <button
          type="button"
          onClick={onClose}
          aria-label={t('rightSidebar.close')}
          title={t('rightSidebar.close')}
          className="rounded-lg p-1.5 text-claude-textSecondary transition-colors hover:bg-claude-btn-hover hover:text-claude-text"
        >
          <X size={18} />
        </button>
      </div>
      <div
        role="tabpanel"
        id={`${RIGHT_SIDEBAR_ID}-panel-${tab}`}
        aria-labelledby={`${RIGHT_SIDEBAR_ID}-tab-${tab}`}
        className="min-h-0 flex-1 overflow-auto"
      >
        <Panel />
      </div>
    </aside>
  );
}
