export type BrowserUrlResult =
  | { ok: true; url: string }
  | { ok: false; error: 'empty' | 'invalid' | 'unsupported' | 'self' };

/** Static shortcuts for common local dev servers; nothing is probed. */
export const LOCAL_DEV_SUGGESTIONS = [
  'http://localhost:5173',
  'http://localhost:3000',
  'http://localhost:8080',
  'http://localhost:4200',
  'http://localhost:8000',
] as const;

/** Hosts that serve this app or its Tauri IPC/asset protocols on Windows. Framing them would be same-origin with the app. */
const APP_HOSTS = new Set(['tauri.localhost', 'ipc.localhost', 'asset.localhost']);

function stripBrackets(host: string): string {
  return host.startsWith('[') && host.endsWith(']') ? host.slice(1, -1) : host;
}

/** Loopback hosts: the CSP allows framing them, and dev servers rarely forbid it. */
export function isLoopbackHost(hostname: string): boolean {
  const host = stripBrackets(hostname.toLowerCase());
  return host === 'localhost' || host.endsWith('.localhost') || host === '::1' || /^127(?:\.\d{1,3}){3}$/.test(host);
}

/** Loopback, private IPv4 ranges and mDNS names: plain-HTTP dev servers on this machine or the LAN. */
export function isLocalNetworkHost(hostname: string): boolean {
  const host = stripBrackets(hostname.toLowerCase());
  if (isLoopbackHost(host) || host.endsWith('.local') || host === '0.0.0.0') return true;
  const ipv4 = host.match(/^(\d{1,3})\.(\d{1,3})\.\d{1,3}\.\d{1,3}$/);
  if (!ipv4) return false;
  const [a, b] = [Number(ipv4[1]), Number(ipv4[2])];
  return a === 10 || (a === 192 && b === 168) || (a === 172 && b >= 16 && b <= 31) || (a === 169 && b === 254);
}

export function isLoopbackUrl(url: string): boolean {
  try {
    return isLoopbackHost(new URL(url).hostname);
  } catch {
    return false;
  }
}

function currentAppOrigin(): string | undefined {
  if (typeof window === 'undefined') return undefined;
  const { origin } = window.location;
  return /^https?:\/\//i.test(origin) ? origin : undefined;
}

/**
 * Turn address-bar input into a URL the embedded browser may load.
 * Bare hosts get http:// on loopback/LAN and https:// elsewhere. Only http(s) and
 * about:blank are accepted; the app's own origin is refused because a same-origin
 * frame with allow-scripts and allow-same-origin could lift its own sandbox.
 */
export function normalizeBrowserUrl(input: string, appOrigin: string | undefined = currentAppOrigin()): BrowserUrlResult {
  const raw = input.trim();
  if (!raw) return { ok: false, error: 'empty' };
  if (/^about:blank$/i.test(raw)) return { ok: true, url: 'about:blank' };
  if (/\s/.test(raw)) return { ok: false, error: 'invalid' };

  let candidate: string;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(raw)) {
    candidate = raw;
  } else if (/^[a-z][a-z0-9+.-]*:(?!\d+(?:[/?#]|$))/i.test(raw)) {
    // A scheme without "//" (javascript:, data:, about:, mailto:…), not "host:port".
    return { ok: false, error: 'unsupported' };
  } else {
    const authority = raw.replace(/^\/\//, '').split(/[/?#]/)[0];
    const host = authority.startsWith('[') ? authority.slice(0, authority.indexOf(']') + 1) : authority.split(':')[0];
    candidate = `${isLocalNetworkHost(host) ? 'http' : 'https'}://${raw.replace(/^\/\//, '')}`;
  }

  let parsed: URL;
  try {
    parsed = new URL(candidate);
  } catch {
    return { ok: false, error: 'invalid' };
  }
  if (parsed.protocol !== 'http:' && parsed.protocol !== 'https:') return { ok: false, error: 'unsupported' };
  if (!parsed.hostname) return { ok: false, error: 'invalid' };
  if (APP_HOSTS.has(parsed.hostname.toLowerCase()) || (appOrigin && parsed.origin === appOrigin)) {
    return { ok: false, error: 'self' };
  }
  return { ok: true, url: parsed.href };
}
