import { create } from 'zustand';

/** Browser features that pages served over plain HTTP cannot use. */
export type SecureFeature = 'clipboard' | 'microphone';

interface SecureContextNotice {
  feature: SecureFeature | null;
  show: (feature: SecureFeature) => void;
  close: () => void;
}

export const useSecureContextNotice = create<SecureContextNotice>((set) => ({
  feature: null,
  show: (feature) => set({ feature }),
  close: () => set({ feature: null }),
}));

/** False on pages served over plain HTTP from an address other than localhost. */
export function isSecureContext(): boolean {
  return typeof window === 'undefined' || window.isSecureContext !== false;
}

/**
 * Returns true when `feature` can be used here. Otherwise shows a dialog that
 * explains the HTTPS requirement and returns false.
 */
export function requireSecureContext(feature: SecureFeature): boolean {
  if (isSecureContext()) return true;
  useSecureContextNotice.getState().show(feature);
  return false;
}

/** Shows the HTTPS dialog for `feature` when the page is not a secure context. */
export function reportInsecureContext(feature: SecureFeature): void {
  if (!isSecureContext()) useSecureContextNotice.getState().show(feature);
}
