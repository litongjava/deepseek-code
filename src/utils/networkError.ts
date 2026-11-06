import { translate } from '../locales';
import { useUIStore } from '../stores/useUIStore';

// Browser wording for a request that never got an HTTP response:
// Chrome "Failed to fetch" / "network error", Firefox "NetworkError when
// attempting to fetch resource.", Safari "Load failed".
const NETWORK_FAILURE = /failed to fetch|networkerror|^(?:typeerror:\s*)?network error|load failed|err_(?:network|connection|internet)/i;

/** True when a request failed before any response: offline, server down or connection dropped. */
export function isNetworkFailure(error: unknown): boolean {
  const text = error instanceof Error ? error.message : typeof error === 'string' ? error : '';
  return NETWORK_FAILURE.test(text.trim());
}

/** The user-facing message for a request that could not reach the server. */
export function networkFailureMessage(): string {
  return translate(useUIStore.getState().language, 'network.unreachable');
}

/** An error message suitable for display, replacing raw browser network errors. */
export function describeError(error: unknown): string {
  if (isNetworkFailure(error)) return networkFailureMessage();
  return error instanceof Error ? error.message : String(error);
}
