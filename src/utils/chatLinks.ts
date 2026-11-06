import { open } from '@tauri-apps/plugin-shell';
import { useGeneralSettingsStore } from '../stores/useGeneralSettingsStore';
import { hasNativeDesktop } from '../services/bridgeConnection';

export async function openChatLink(url: string): Promise<void> {
  if (!/^https?:\/\//i.test(url)) return;
  if (useGeneralSettingsStore.getState().chatLinks === 'sidebar') {
    window.dispatchEvent(new CustomEvent('request-open-panel', { detail: { panel: 'browser', url } }));
  } else if (hasNativeDesktop()) {
    await open(url);
  } else {
    window.open(url, '_blank', 'noopener,noreferrer');
  }
}
