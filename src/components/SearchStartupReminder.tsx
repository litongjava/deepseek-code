import React, { useEffect, useState } from 'react';
import { useLocation, useNavigate } from 'react-router-dom';
import { X } from 'lucide-react';
import { getSearchSettings, type SearchSettings } from '../api';

export const WEB_SEARCH_REMINDER_LINE =
  'Web search not configured · run /search-config to set up DeepSeek, OpenAI, or Anthropic';

export function isSearchConfigured(settings: SearchSettings): boolean {
  return Boolean(
    settings.baseUrl?.trim() &&
      settings.model?.trim() &&
      settings.hasApiKey
  );
}

export default function SearchStartupReminder() {
  const location = useLocation();
  const navigate = useNavigate();
  const [missing, setMissing] = useState(false);
  const [dismissed, setDismissed] = useState(false);

  const isSettingsPage = location.pathname === '/settings';

  useEffect(() => {
    if (isSettingsPage || dismissed) return;
    let cancelled = false;
    getSearchSettings()
      .then((settings) => {
        if (!cancelled) {
          setMissing(!isSearchConfigured(settings));
        }
      })
      .catch(() => {
        // Ignore transient startup/bridge errors; never show a false alarm.
      });
    return () => {
      cancelled = true;
    };
  }, [isSettingsPage, dismissed, location.pathname]);

  if (isSettingsPage || dismissed || !missing) {
    return null;
  }

  return (
    <div
      role="status"
      className="mx-4 mt-2 mb-1 px-3.5 py-2 rounded-lg border border-amber-500/30 bg-amber-500/10 text-deepseek-text text-xs flex items-start justify-between gap-3 shrink-0"
    >
      <div className="min-w-0">
        <div className="font-semibold text-amber-600 dark:text-amber-400">System Reminder</div>
        <div className="mt-0.5 text-deepseek-textSecondary">
          - {WEB_SEARCH_REMINDER_LINE}
        </div>
      </div>
      <div className="flex items-center gap-1.5 shrink-0 pt-0.5">
        <button
          type="button"
          onClick={() => navigate('/settings?tab=search')}
          className="px-2.5 py-1 rounded-md bg-deepseek-input hover:bg-black/5 dark:hover:bg-white/10 border border-deepseek-border text-deepseek-text font-medium transition-colors"
        >
          Configure
        </button>
        <button
          type="button"
          aria-label="Dismiss reminder"
          onClick={() => setDismissed(true)}
          className="p-1 rounded-md text-deepseek-textSecondary hover:text-deepseek-text hover:bg-black/5 dark:hover:bg-white/10 transition-colors"
        >
          <X size={14} />
        </button>
      </div>
    </div>
  );
}
