import React, { useEffect, useId, useState } from 'react';
import { useI18n } from '../../../hooks/useI18n';
import { normalizeBrowserUrl, type BrowserUrlResult } from './browserUrl';

const errorKeys: Record<Extract<BrowserUrlResult, { ok: false }>['error'], string> = {
  empty: 'rightSidebar.browserInvalidUrl',
  invalid: 'rightSidebar.browserInvalidUrl',
  unsupported: 'rightSidebar.browserUnsupportedUrl',
  self: 'rightSidebar.browserSelfUrl',
};

/** URL input that normalizes on Enter and reports rejected input inline. */
export default function AddressBar({ value, onNavigate, autoFocus = false, className = '', children }: {
  /** The page currently shown; the draft resets to it when it changes. */
  value: string;
  onNavigate: (url: string) => void;
  autoFocus?: boolean;
  className?: string;
  children?: React.ReactNode;
}) {
  const { t } = useI18n();
  const errorId = useId();
  const [draft, setDraft] = useState(value);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    setDraft(value);
    setError(null);
  }, [value]);

  const submit = (event: React.FormEvent) => {
    event.preventDefault();
    const result = normalizeBrowserUrl(draft);
    if (!result.ok) {
      setError(t(errorKeys[result.error]));
      return;
    }
    setError(null);
    setDraft(result.url);
    onNavigate(result.url);
  };

  return (
    <form onSubmit={submit} className={`min-w-0 ${className}`}>
      <div className="flex min-w-0 items-center gap-1">
        <input
          type="text"
          inputMode="url"
          spellCheck={false}
          autoCapitalize="off"
          autoCorrect="off"
          autoFocus={autoFocus}
          value={draft}
          onChange={event => { setDraft(event.target.value); setError(null); }}
          onKeyDown={event => {
            if (event.key === 'Escape' && draft !== value) {
              event.stopPropagation();
              setDraft(value);
              setError(null);
            }
          }}
          onFocus={event => event.currentTarget.select()}
          aria-label={t('rightSidebar.browserAddress')}
          aria-invalid={!!error}
          aria-describedby={error ? errorId : undefined}
          placeholder={t('rightSidebar.browserAddressPlaceholder')}
          className={`min-w-0 flex-1 rounded-md border bg-claude-input px-2 py-1 text-[13px] text-claude-text placeholder-claude-textSecondary/60 outline-none transition-colors focus:border-claude-textSecondary/60 ${error ? 'border-red-500/70' : 'border-claude-border'}`}
        />
        {children}
      </div>
      {error && <p id={errorId} role="alert" className="mt-1 text-[12px] text-red-600 dark:text-red-400">{error}</p>}
    </form>
  );
}
