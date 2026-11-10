import React, { useCallback, useEffect, useState } from 'react';
import {
  connectionMode, isMobileApp, loginToServer, onLoginRequired, savedServerUrl,
  ServerLoginError, switchToLocalEngine, waitForApiReady, type ServerLoginCode,
} from '../../services/bridgeConnection';
import { useI18n } from '../../hooks/useI18n';

type GateState =
  | { kind: 'connecting' }
  | { kind: 'ready' }
  | { kind: 'login'; askServer: boolean; code: ServerLoginCode; detail?: string };

/**
 * Connects to a dcd-server before showing the app in a browser or the mobile app.
 * Desktop, the development proxy and the mobile app's own engine connect on their
 * own and pass straight through; the mobile app can still switch to a server.
 */
export default function ServerGate({ children }: { children: React.ReactNode }) {
  const webMode = connectionMode() === 'web';
  const [state, setState] = useState<GateState>(webMode ? { kind: 'connecting' } : { kind: 'ready' });

  const connect = useCallback(() => {
    setState({ kind: 'connecting' });
    waitForApiReady().then(
      () => setState({ kind: 'ready' }),
      (error: unknown) => setState(loginState(error)),
    );
  }, []);

  useEffect(() => {
    if (!webMode && !isMobileApp) return;
    const unsubscribe = onLoginRequired(error => setState(loginState(error)));
    if (webMode) connect();
    return unsubscribe;
  }, [webMode, connect]);

  if (state.kind === 'ready') return <>{children}</>;
  if (state.kind === 'connecting') return <Centered><ConnectingText /></Centered>;
  return <Centered><ServerLoginForm state={state} onConnected={connect} /></Centered>;
}

function loginState(error: unknown): GateState {
  const code = error instanceof ServerLoginError ? error.code : 'unreachable';
  const askServer = isMobileApp || !!savedServerUrl() || code === 'needsServer'
    || (error as { needsServerUrl?: boolean })?.needsServerUrl === true;
  // The message carries a detail (the server address) only when it differs from the code.
  const detail = error instanceof ServerLoginError && error.message !== error.code ? error.message : undefined;
  return { kind: 'login', askServer, code, detail };
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex items-center justify-center w-full min-h-screen bg-deepseek-bg px-4 py-8"
      style={{ paddingTop: 'max(2rem, env(safe-area-inset-top))', paddingBottom: 'max(2rem, env(safe-area-inset-bottom))' }}>
      {children}
    </div>
  );
}

function ConnectingText() {
  const { t } = useI18n();
  return <div className="text-deepseek-textSecondary text-[14px]">{t('serverLogin.connecting')}</div>;
}

function ServerLoginForm({ state, onConnected }: {
  state: Extract<GateState, { kind: 'login' }>;
  onConnected: () => void;
}) {
  const { t } = useI18n();
  const [serverUrl, setServerUrl] = useState(savedServerUrl() || '');
  const [showServer, setShowServer] = useState(state.askServer);
  const [token, setToken] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<{ code: ServerLoginCode; detail?: string } | null>(
    state.code === 'needsToken' || state.code === 'needsServer' ? null : { code: state.code, detail: state.detail },
  );

  const submit = async (event: React.FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await loginToServer(token, showServer ? serverUrl : undefined);
      onConnected();
    } catch (cause) {
      setError(cause instanceof ServerLoginError
        ? { code: cause.code, detail: cause.message !== cause.code ? cause.message : undefined }
        : { code: 'failed' });
    } finally {
      setBusy(false);
    }
  };

  const input = 'w-full rounded-lg border border-deepseek-border bg-deepseek-input px-3 py-2.5 text-[15px] text-deepseek-text outline-none focus:border-deepseek-accent';
  return (
    <form onSubmit={submit} className="w-full max-w-sm space-y-4" aria-label={t('serverLogin.title')}>
      <div className="space-y-1">
        <h1 className="text-xl font-semibold text-deepseek-text">{t('serverLogin.title')}</h1>
        <p className="text-[14px] text-deepseek-textSecondary">{t('serverLogin.description')}</p>
      </div>
      {showServer && (
        <label className="block space-y-1">
          <span className="text-[13px] text-deepseek-textSecondary">{t('serverLogin.serverUrl')}</span>
          <input className={input} type="url" inputMode="url" autoCapitalize="off" autoCorrect="off" required
            placeholder="https://server.example:30085" value={serverUrl} onChange={e => setServerUrl(e.target.value)} />
        </label>
      )}
      <div className="space-y-1">
        <label className="block space-y-1">
          <span className="text-[13px] text-deepseek-textSecondary">{t('serverLogin.token')}</span>
          <input className={input} type="password" autoComplete="current-password" autoCapitalize="off" required
            aria-describedby="server-token-hint" value={token} onChange={e => setToken(e.target.value)} />
        </label>
        <p id="server-token-hint" className="text-[12px] text-deepseek-textSecondary">{t('serverLogin.tokenHint')}</p>
      </div>
      {error && (
        <p role="alert" className="text-[13px] text-red-500">
          {t(`serverLogin.errors.${error.code}`)}{error.code === 'unreachable' && error.detail ? ` ${error.detail}` : ''}
        </p>
      )}
      <button type="submit" disabled={busy || !token.trim()}
        className="w-full rounded-lg bg-deepseek-accent px-4 py-2.5 text-[15px] font-medium text-white disabled:opacity-50">
        {busy ? t('serverLogin.connecting') : t('serverLogin.submit')}
      </button>
      {!showServer && (
        <button type="button" className="w-full text-[13px] text-deepseek-textSecondary underline" onClick={() => setShowServer(true)}>
          {t('serverLogin.otherServer')}
        </button>
      )}
      {isMobileApp && (
        <button type="button" className="w-full text-[13px] text-deepseek-textSecondary underline"
          onClick={() => { switchToLocalEngine(); window.location.reload(); }}>
          {t('serverLogin.useThisPhone')}
        </button>
      )}
    </form>
  );
}
