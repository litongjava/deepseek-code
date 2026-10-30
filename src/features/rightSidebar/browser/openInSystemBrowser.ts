import { tauriAPI } from '../../../utils/tauriAPI';

/** Open a page in the user's default browser (Desktop) or a new tab (plain web build). */
export async function openInSystemBrowser(url: string): Promise<void> {
  if (tauriAPI.isTauri) {
    try {
      await tauriAPI.openExternal(url);
    } catch (error) {
      console.warn('Failed to open the system browser', error);
    }
    return;
  }
  window.open(url, '_blank', 'noopener,noreferrer');
}
