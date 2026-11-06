import React, { useEffect, useState } from 'react';
import { open } from '@tauri-apps/plugin-dialog';
import { getShellSettings, saveShellSettings, getWslDistributions, type ShellConfiguration, type ShellSettingsStatus } from '../api';
import { isTauriApp } from '../services/bridgeConnection';
import { useI18n } from '../hooks/useI18n';
import { getErrorMessage } from '../utils/errorHelpers';

const shells = ['bash', 'powershell', 'pwsh', 'cmd', 'sh', 'zsh', 'fish', 'wsl'];

export default function ShellSettings() {
  const { t } = useI18n();
  const [status, setStatus] = useState<ShellSettingsStatus | null>(null);
  const [settings, setSettings] = useState<ShellConfiguration | null>(null);
  const [distributions, setDistributions] = useState<string[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  useEffect(() => {
    let active = true;
    getShellSettings().then(value => {
      if (active) { setStatus(value); setSettings(value.settings); }
    }).catch(reason => { if (active) setError(getErrorMessage(reason)); });
    return () => { active = false; };
  }, []);
  const edit = (patch: Partial<ShellConfiguration>) => {
    setSettings(current => current && { ...current, ...patch });
    setNotice('');
  };
  const save = async (event: React.FormEvent) => {
    event.preventDefault();
    if (!settings) return;
    setBusy(true); setError(''); setNotice('');
    try {
      const value = await saveShellSettings(settings);
      setStatus(value); setSettings(value.settings); setNotice(t('shellSettings.saved'));
    } catch (reason) { setError(getErrorMessage(reason)); }
    finally { setBusy(false); }
  };
  const browse = async () => {
    setError('');
    try {
      const selected = await open({ directory: false, multiple: false, title: t('shellSettings.bashPath') });
      if (typeof selected === 'string') edit({ bashPath: selected });
    } catch (reason) { setError(getErrorMessage(reason)); }
  };
  const detect = async () => {
    setBusy(true); setError('');
    try {
      const values = await getWslDistributions();
      setDistributions(values);
      if (!values.length) setError(t('shellSettings.noDistributions'));
    } catch (reason) { setError(getErrorMessage(reason)); }
    finally { setBusy(false); }
  };
  const control = 'w-full rounded-lg border border-claude-border bg-claude-bg px-3 py-2 text-sm';
  const wsl = settings?.toolDefault === 'wsl' || settings?.terminalDefault === 'wsl';
  return <form aria-label={t('shellSettings.title')} onSubmit={save} className="border-b border-claude-border py-6">
    <h3 className="text-[16px]">{t('shellSettings.title')}</h3>
    <p className="mt-2 text-sm text-claude-textSecondary">{t('shellSettings.description')}</p>
    {error && <p role="alert" className="mt-3 text-sm text-red-500">{error}</p>}
    {!settings ? !error && <p className="mt-3 text-sm">{t('common.loading')}</p> : <fieldset disabled={busy} className="mt-4 space-y-4 disabled:opacity-60">
      <div className="grid gap-4 sm:grid-cols-2">
        {(['toolDefault', 'terminalDefault'] as const).map(key => <label key={key} className="space-y-2 text-sm">
          <span>{t(`shellSettings.${key}`)}</span>
          <select className={control} value={settings[key]} onChange={event => edit({ [key]: event.target.value })}>
            {key === 'terminalDefault' && <option value="auto">{t('shellSettings.auto')}</option>}
            {shells.map(shell => <option key={shell} value={shell}>{shell === 'wsl' ? 'WSL' : shell}</option>)}
          </select>
        </label>)}
      </div>
      <label className="block space-y-2 text-sm"><span>{t('shellSettings.bashPath')}</span>
        <input className={control} value={settings.bashPath || ''} placeholder={t('shellSettings.detected')} onChange={event => edit({ bashPath: event.target.value || null })} />
      </label>
      {isTauriApp && <button type="button" onClick={() => void browse()} className="rounded-lg border border-claude-border px-3 py-2 text-sm">{t('shellSettings.browse')}</button>}
      <p className="text-xs text-claude-textSecondary">{t('shellSettings.precedence')}</p>
      {wsl && <div className="space-y-3 rounded-lg border border-claude-border p-3">
        <label className="block space-y-2 text-sm"><span>{t('shellSettings.distribution')}</span>
          <input required className={control} list="shell-wsl-distributions" value={settings.wsl.distribution || ''} onChange={event => edit({ wsl: { ...settings.wsl, distribution: event.target.value || null } })} />
        </label>
        <datalist id="shell-wsl-distributions">{distributions.map(value => <option key={value} value={value}>{value}</option>)}</datalist>
        <button type="button" className="rounded-lg border border-claude-border px-3 py-2 text-sm" onClick={() => void detect()}>{t('shellSettings.detectWsl')}</button>
        <label className="block space-y-2 text-sm"><span>{t('shellSettings.wslBash')}</span>
          <input required className={control} value={settings.wsl.bashPath} onChange={event => edit({ wsl: { ...settings.wsl, bashPath: event.target.value } })} />
        </label>
      </div>}
      {status && <div className="space-y-1 break-all text-xs text-claude-textSecondary">
        {(['tool', 'terminal'] as const).map(key => <p key={key}>{t(`shellSettings.${key}`)}: {status[key].error || `${status[key].program} (${t(`shellSettings.${status[key].source}`)})`}</p>)}
      </div>}
      <div className="flex items-center justify-between gap-3">
        <span aria-live="polite" className="text-sm text-claude-textSecondary">{notice}</span>
        <button type="submit" className="rounded-lg bg-claude-text px-4 py-2 text-sm text-claude-bg">{t('common.save')}</button>
      </div>
    </fieldset>}
  </form>;
}
