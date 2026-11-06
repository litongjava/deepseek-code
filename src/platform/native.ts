import { hasNativeDesktop } from '../services/bridgeConnection';

/** Raised for a Desktop command that has no equivalent in the browser or the mobile app. */
export class DesktopOnlyError extends Error {
  constructor(public readonly command: string) {
    super('This feature requires the DeepSeek Code Desktop app.');
    this.name = 'DesktopOnlyError';
  }
}

type Args = Record<string, unknown>;

async function postBridge(path: string, body: unknown): Promise<unknown> {
  const { request } = await import('../api/client');
  const response = await request(path, { method: 'POST', body: JSON.stringify(body) });
  return response.json();
}

/**
 * Commands served without the Desktop window: Bridge routes on a dcd-server,
 * or browser APIs. Keep each entry's result shape identical to the Tauri command.
 */
const webFallbacks: Record<string, (args: Args) => Promise<unknown>> = {
  peripheral_command: args => postBridge('/peripheral', { request: args.request }),
  open_external_url: async args => { window.open(String(args.url), '_blank', 'noopener,noreferrer'); },
};

export function hasWebFallback(command: string): boolean {
  return command in webFallbacks;
}

/**
 * Calls a Desktop (Tauri) command. In a browser or the mobile app it uses the web
 * fallback when one exists and otherwise rejects with `DesktopOnlyError`, so
 * callers can show a clear message instead of failing on a missing Tauri runtime.
 */
export async function invokeDesktop<T = unknown>(command: string, args?: Args): Promise<T> {
  if (hasNativeDesktop()) {
    const { invoke } = await import('@tauri-apps/api/core');
    return invoke<T>(command, args);
  }
  const fallback = webFallbacks[command];
  if (fallback) return (await fallback(args ?? {})) as T;
  throw new DesktopOnlyError(command);
}
