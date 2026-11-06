import React, { useState } from 'react';
import { connectGithubToken } from '../../api/github';
import { useI18n } from '../../hooks/useI18n';

/** Connects GitHub with a personal access token when OAuth is not configured. */
export default function GithubTokenForm({ onConnected }: {
  onConnected: (user: { login: string; avatar_url: string; name?: string } | null) => void;
}) {
  const { t } = useI18n();
  const [token, setToken] = useState('');
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setFailed(false);
    try {
      const result = await connectGithubToken(token.trim());
      setToken('');
      onConnected(result?.user ?? null);
    } catch {
      setFailed(true);
    } finally {
      setBusy(false);
    }
  };

  return (
    <form onSubmit={submit} className="w-full max-w-sm space-y-2 text-left">
      <label className="block space-y-1">
        <span className="text-[12px] text-claude-textSecondary">{t('githubToken.tokenLabel')}</span>
        <input type="password" autoComplete="off" value={token} onChange={e => setToken(e.target.value)}
          className="w-full rounded-lg border border-claude-border bg-claude-input px-3 py-1.5 text-[13px] text-claude-text outline-none" />
      </label>
      <p className="text-[11px] text-claude-textSecondary">{t('githubToken.tokenHint')}</p>
      {failed && <p role="alert" className="text-[12px] text-red-500">{t('githubToken.tokenFailed')}</p>}
      <button type="submit" disabled={busy || !token.trim()}
        className="px-4 py-1.5 bg-black text-white dark:bg-white dark:text-black rounded-lg text-[13px] disabled:opacity-50">
        {t('githubToken.connect')}
      </button>
    </form>
  );
}
