import { useEffect, useState, type FormEvent } from 'react';
import { Copy, Eye, EyeOff } from 'lucide-react';
import { useI18n } from '../hooks/useI18n';
import { copyToClipboard } from '../utils/clipboard';
import { getSearchSettings, getSearchApiKey, updateSearchSettings, testSearchSettings, type SearchSettings as SearchSettingsData, type SearchSettingsUpdate, type SearchSettingsTestResult } from '../api';

const inputClass = 'w-full bg-transparent border border-deepseek-border rounded-lg px-3 py-2 text-[13px] text-deepseek-text outline-none focus:border-[#387ee0]';
const defaults: SearchSettingsData = { baseUrl: 'https://api.deepseek.com/anthropic/v1', model: 'deepseek-flash', hasApiKey: false };

export default function SearchSettings() {
  const { t } = useI18n();
  const [settings, setSettings] = useState(defaults);
  const [apiKey, setApiKey] = useState('');
  const [showKey, setShowKey] = useState(false);
  const [revealedKey, setRevealedKey] = useState('');
  const [keyBusy, setKeyBusy] = useState(false);
  const [clearApiKey, setClearApiKey] = useState(false);
  const [loaded, setLoaded] = useState(false);
  const [busy, setBusy] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<SearchSettingsTestResult | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');

  const load = async () => {
    setBusy(true); setError('');
    try {
      const result = await getSearchSettings();
      setSettings({ baseUrl: result.baseUrl, model: result.model, hasApiKey: result.hasApiKey });
      setLoaded(true);
    } catch (err) { setError((err as Error).message); }
    finally { setBusy(false); }
  };
  useEffect(() => { void load(); }, []);
  const hideKey = () => { setShowKey(false); setRevealedKey(''); };
  const keyValue = async () => apiKey.trim() || revealedKey || await getSearchApiKey();
  const toggleKey = async () => {
    if (showKey) { hideKey(); return; }
    setKeyBusy(true); setError(''); setNotice('');
    try {
      if (!apiKey.trim()) setRevealedKey(await keyValue());
      setShowKey(true);
    } catch { setError(t('searchSettings.readKeyError')); }
    finally { setKeyBusy(false); }
  };
  const copyKey = async () => {
    setKeyBusy(true); setError(''); setNotice('');
    try {
      let key: string;
      try { key = await keyValue(); }
      catch { setError(t('searchSettings.readKeyError')); return; }
      if (await copyToClipboard(key)) setNotice(t('searchSettings.copied'));
      else setError(t('searchSettings.copyError'));
    } catch { setError(t('searchSettings.copyError')); }
    finally { setKeyBusy(false); }
  };
  const edited = () => { setNotice(''); setTestResult(null); setError(''); };
  const currentSettings = (): SearchSettingsUpdate => ({
    baseUrl: settings.baseUrl.trim(), model: settings.model.trim(),
    ...(clearApiKey ? { clearApiKey: true } : apiKey.trim() ? { apiKey: apiKey.trim() } : {}),
  });
  const testSearch = async () => {
    setTesting(true); setError(''); setTestResult(null); setNotice('');
    try { setTestResult(await testSearchSettings(currentSettings())); }
    catch (err) { setError((err as Error).message); }
    finally { setTesting(false); }
  };

  const save = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    hideKey();
    setBusy(true); setError(''); setNotice('');
    try {
      const result = await updateSearchSettings(currentSettings());
      setSettings({ baseUrl: result.baseUrl, model: result.model, hasApiKey: result.hasApiKey });
      setApiKey(''); setClearApiKey(false);
      setNotice(t('searchSettings.saved'));
    } catch (err) { setError((err as Error).message); }
    finally { setBusy(false); }
  };

  return <section className="max-w-2xl space-y-5">
    <div>
      <h2 className="text-xl font-semibold text-deepseek-text">{t('searchSettings.title')}</h2>
      <p className="mt-2 text-sm text-deepseek-textSecondary">{t('searchSettings.description')}</p>
    </div>
    {error && <div role="alert" className="rounded-lg bg-red-500/10 text-red-500 p-3 text-sm">{error}</div>}
    {!loaded && !busy && <button type="button" onClick={() => void load()} className="text-sm text-deepseek-text underline">{t('searchSettings.reload')}</button>}
    <form onSubmit={save} className="space-y-5 rounded-2xl border border-deepseek-border bg-deepseek-input p-6">
      <fieldset disabled={!loaded || busy || testing || keyBusy} className="space-y-5 disabled:opacity-60">
        <label className="block text-xs text-deepseek-textSecondary">{t('searchSettings.baseUrl')}<input required type="url" aria-label={t('searchSettings.baseUrl')} value={settings.baseUrl} onChange={event => { setSettings({ ...settings, baseUrl: event.target.value }); edited(); }} className={`${inputClass} mt-1 font-mono`} placeholder={defaults.baseUrl} /></label>
        <label className="block text-xs text-deepseek-textSecondary">{t('searchSettings.model')}<input required aria-label={t('searchSettings.model')} value={settings.model} onChange={event => { setSettings({ ...settings, model: event.target.value }); edited(); }} className={`${inputClass} mt-1`} placeholder={defaults.model} /></label>
        <div>
          <label htmlFor="search-api-key" className="block text-xs text-deepseek-textSecondary">{t('searchSettings.apiKey')}</label>
          <div className="mt-1 flex items-center gap-2">
            <input id="search-api-key" type={showKey ? 'text' : 'password'} aria-label={t('searchSettings.apiKey')} autoComplete="new-password" value={apiKey || (showKey ? revealedKey : '')} disabled={clearApiKey} onChange={event => { setApiKey(event.target.value); setRevealedKey(''); edited(); }} className={inputClass} placeholder={settings.hasApiKey ? t('searchSettings.savedPlaceholder') : t('searchSettings.newPlaceholder')} />
            <button type="button" aria-label={showKey ? t('searchSettings.hideKey') : t('searchSettings.showKey')} title={showKey ? t('searchSettings.hideKey') : t('searchSettings.showKey')} disabled={clearApiKey || (!apiKey.trim() && !settings.hasApiKey)} onClick={() => void toggleKey()} className="rounded p-2 text-deepseek-textSecondary hover:bg-deepseek-hover disabled:opacity-40">{showKey ? <EyeOff size={16} /> : <Eye size={16} />}</button>
            <button type="button" aria-label={t('searchSettings.copyKey')} title={t('searchSettings.copyKey')} disabled={clearApiKey || (!apiKey.trim() && !settings.hasApiKey)} onClick={() => void copyKey()} className="rounded p-2 text-deepseek-textSecondary hover:bg-deepseek-hover disabled:opacity-40"><Copy size={16} /></button>
          </div>
          <p className="mt-2 text-xs text-deepseek-textSecondary">{settings.hasApiKey ? t('searchSettings.keyConfigured') : t('searchSettings.keyMissing')}</p>
          {settings.hasApiKey && <label className="mt-3 flex items-center gap-2 text-xs text-deepseek-textSecondary"><input type="checkbox" checked={clearApiKey} onChange={event => { setClearApiKey(event.target.checked); setApiKey(''); hideKey(); edited(); }} />{t('searchSettings.clearKey')}</label>}
        </div>
        <p className="text-xs text-deepseek-textSecondary">{t('searchSettings.testHint')}</p>
        <div className="flex items-center justify-end gap-3 border-t border-deepseek-border pt-4">
          <span role="status" className="flex-1 text-xs text-deepseek-textSecondary">{notice}</span>
          <button type="button" onClick={event => { if (event.currentTarget.form?.reportValidity()) void testSearch(); }} className="rounded-lg border border-deepseek-border px-4 py-2 text-sm text-deepseek-text">{testing ? t('searchSettings.testing') : t('searchSettings.test')}</button>
          <button type="submit" className="rounded-lg bg-deepseek-text px-4 py-2 text-sm text-deepseek-bg">{busy ? t('searchSettings.saving') : t('searchSettings.save')}</button>
        </div>
      </fieldset>
    </form>
    {testResult && <div className="space-y-3 rounded-lg border border-deepseek-border p-4 text-sm">
      <p role="status" className="text-deepseek-text">{t('searchSettings.testSuccess', { count: testResult.resultCount, seconds: (testResult.elapsedMs / 1000).toFixed(1), model: testResult.model })}</p>
      {testResult.results.filter(result => /^https?:\/\//i.test(result.url)).slice(0, 3).map((result, index) => <div key={`${result.url}-${index}`}>
        <a href={result.url} target="_blank" rel="noopener noreferrer" className="text-blue-500 hover:underline">{result.title || result.url}</a>
        {result.snippet && <p className="mt-1 line-clamp-2 text-xs text-deepseek-textSecondary">{result.snippet}</p>}
      </div>)}
    </div>}
  </section>;
}
