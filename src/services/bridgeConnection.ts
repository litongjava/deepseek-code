const DEFAULT_BRIDGE_PORT = 30085;
const STARTUP_TIMEOUT_MS = 300_000;
const RETRY_INTERVAL_MS = 1500;
const SERVER_URL_KEY = 'dcd_server_url';
const ACCESS_TOKEN_KEY = 'dcd_access_token';

export const isTauriApp = typeof window !== 'undefined' && !!(window as any).__TAURI_INTERNALS__;
const userAgent = typeof navigator !== 'undefined' ? navigator.userAgent : '';
/** Tauri on Android or iOS: there is no local Bridge, so the app connects to a server. */
export const isMobileApp = isTauriApp && /Android|iPhone|iPad|iPod/i.test(userAgent);
export const useDevProxy = import.meta.env.DEV && !isTauriApp;

/**
 * How this page reaches the Bridge:
 * - desktop: the Bridge started by this Desktop process on loopback;
 * - local: the Bridge running inside the mobile app on this phone;
 * - dev-proxy: the Vite development server proxies `/api`;
 * - web: a dcd-server, either the origin that served this page or a saved server URL.
 */
export type ConnectionMode = 'desktop' | 'local' | 'dev-proxy' | 'web';

export function savedServerUrl(): string | null {
  try { return localStorage.getItem(SERVER_URL_KEY); } catch { return null; }
}

const tauriPresent = () => typeof window !== 'undefined' && !!(window as any).__TAURI_INTERNALS__;

export function connectionMode(): ConnectionMode {
  const tauri = tauriPresent();
  // The mobile app runs its own engine until the user connects it to a server.
  if (tauri && /Android|iPhone|iPad|iPod/i.test(userAgent)) return savedServerUrl() ? 'web' : 'local';
  if (tauri) return 'desktop';
  if (useDevProxy && !savedServerUrl()) return 'dev-proxy';
  return 'web';
}

/** Whether native Desktop commands (windows, tray, local dialogs) are available. */
export const hasNativeDesktop = () => connectionMode() === 'desktop';

/**
 * Whether a Bridge stores conversations and runs the engine for this page. Every
 * mode has one: Desktop's own, the development proxy, or a dcd-server.
 */
export const hasBridge = isTauriApp || useDevProxy || connectionMode() === 'web' || connectionMode() === 'local';

function normalizeServerUrl(url: string): string {
  const trimmed = url.trim().replace(/\/+$/, '');
  if (!/^https?:\/\//i.test(trimmed)) throw new ServerLoginError('invalidUrl');
  return trimmed;
}

function webOrigin(): string {
  const saved = savedServerUrl();
  if (saved) return saved;
  // The mobile app has no same-origin server; the login screen asks for one.
  if (isMobileApp) return '';
  return typeof location !== 'undefined' ? location.origin : '';
}

function initialApiBase(): string {
  switch (connectionMode()) {
    case 'dev-proxy': return '/api';
    case 'web': return `${webOrigin()}/api`;
    default: return `http://127.0.0.1:${DEFAULT_BRIDGE_PORT}/api`;
  }
}

export let API_BASE = initialApiBase();

/** Bridge origin without the `/api` suffix, for health checks and absolute URLs. */
export function bridgeOrigin(): string {
  if (API_BASE.startsWith('/')) return typeof location !== 'undefined' ? location.origin : '';
  return API_BASE.replace(/\/api$/, '');
}

/** Why a server connection or login failed; the login screen translates the code. */
export type ServerLoginCode = 'needsServer' | 'unreachable' | 'needsToken' | 'expired' | 'invalidUrl' | 'badToken' | 'failed';

export class ServerLoginError extends Error {
  constructor(public readonly code: ServerLoginCode, detail = '') {
    super(detail || code);
    this.name = 'ServerLoginError';
  }
}

/** Raised when a dcd-server needs the access token (or the app needs a server URL). */
export class LoginRequiredError extends ServerLoginError {
  constructor(public readonly needsServerUrl: boolean, code: ServerLoginCode, detail = '') {
    super(code, detail);
    this.name = 'LoginRequiredError';
  }
}

let detectedBridgePort: number | null = null;
let apiReady: Promise<void> | null = null;
const loginListeners = new Set<(error: LoginRequiredError) => void>();

/** Lets the app show its login screen whenever a server rejects the saved token. */
export function onLoginRequired(listener: (error: LoginRequiredError) => void): () => void {
  loginListeners.add(listener);
  return () => loginListeners.delete(listener);
}

function emitLoginRequired(error: LoginRequiredError) {
  loginListeners.forEach(listener => listener(error));
}

export function resetBridgePort(): void {
  detectedBridgePort = null;
  apiReady = null;
  API_BASE = initialApiBase();
}

async function isHealthy(url: string, timeout: number): Promise<boolean> {
  try {
    const response = await fetch(url, { signal: AbortSignal.timeout(timeout), cache: 'no-store' });
    if (!response.ok) return false;
    const health = await response.json();
    return health.status === 'healthy' && health.database === true;
  } catch {
    return false;
  }
}

function portOf(origin: string): number {
  try {
    const url = new URL(origin);
    return Number(url.port) || (url.protocol === 'https:' ? 443 : 80);
  } catch {
    return DEFAULT_BRIDGE_PORT;
  }
}

async function authStatus(origin: string, token: string | null): Promise<{ authenticated: boolean; server_mode: boolean } | null> {
  try {
    const response = await fetch(`${origin}/api/auth/status`, {
      cache: 'no-store',
      headers: token ? { Authorization: `Bearer ${token}` } : {},
      signal: AbortSignal.timeout(5000),
    });
    return response.ok ? await response.json() : null;
  } catch {
    return null;
  }
}

/** Mobile app: stop using the saved server and return to the engine on this phone. */
export function switchToLocalEngine(): void {
  localStorage.removeItem(SERVER_URL_KEY);
  localStorage.removeItem(ACCESS_TOKEN_KEY);
  localStorage.removeItem('bridge_api_key');
  resetBridgePort();
  // Conversation IDs belong to one engine and cannot cross this boundary.
  if (isMobileApp) window.location.hash = '/';
}

async function connectWeb(): Promise<void> {
  const origin = webOrigin();
  if (!origin) throw new LoginRequiredError(true, 'needsServer');
  if (!(await isHealthy(`${origin}/health`, 5000))) {
    throw new LoginRequiredError(isMobileApp || !!savedServerUrl(), 'unreachable', origin);
  }
  const token = localStorage.getItem(ACCESS_TOKEN_KEY);
  const status = await authStatus(origin, token);
  if (status && !status.authenticated) throw new LoginRequiredError(isMobileApp || !!savedServerUrl(), 'needsToken');
  // Existing request helpers send this key as a Bearer token; without one the
  // same-origin session cookie authenticates, so drop any Desktop key left over.
  if (token) localStorage.setItem('bridge_api_key', token);
  else localStorage.removeItem('bridge_api_key');
  API_BASE = `${origin}/api`;
  detectedBridgePort = portOf(origin);
}

async function connect(): Promise<void> {
  if (connectionMode() === 'web') return connectWeb();
  const deadline = Date.now() + STARTUP_TIMEOUT_MS;
  while (Date.now() < deadline) {
    const timeout = Math.max(1, Math.min(3000, deadline - Date.now()));
    if (useDevProxy) {
      if (await isHealthy('/api/health', timeout)) {
        detectedBridgePort = DEFAULT_BRIDGE_PORT;
        return;
      }
    } else {
      // The endpoint and credential must belong to this Desktop process.
      const { invoke } = await import('@tauri-apps/api/core');
      const endpoint = await invoke<[number, string] | null>('get_bridge_endpoint');
      if (endpoint) {
        const [port, key] = endpoint;
        if (!key) throw new Error('未能获取后端连接凭据，请重试。');
        if (await isHealthy(`http://127.0.0.1:${port}/health`, timeout)) {
          localStorage.setItem('bridge_api_key', key);
          API_BASE = `http://127.0.0.1:${port}/api`;
          detectedBridgePort = port;
          return;
        }
      }
    }
    const remaining = deadline - Date.now();
    if (remaining > 0) await new Promise(resolve => setTimeout(resolve, Math.min(RETRY_INTERVAL_MS, remaining)));
  }
  throw new Error('等待后端启动超时，请检查启动终端中的编译或运行错误，然后重试。');
}

export function waitForApiReady(): Promise<void> {
  if (!apiReady) {
    const pending = connect().catch(error => {
      if (apiReady === pending) apiReady = null;
      if (error instanceof LoginRequiredError) emitLoginRequired(error);
      throw error;
    });
    apiReady = pending;
  }
  return apiReady;
}

export async function detectBridgePort(): Promise<number> {
  await waitForApiReady();
  return detectedBridgePort ?? DEFAULT_BRIDGE_PORT;
}

/**
 * Logs in to a dcd-server with its access token. An empty `serverUrl` means the
 * server that served this page. The token is kept for Bearer requests, and the
 * server also sets an HttpOnly cookie for streams and media on the same origin.
 * Requests use the default `same-origin` credentials: cross-origin clients (the
 * mobile app) authenticate with the Bearer token, so the server never has to
 * allow credentialed cross-origin requests.
 */
export async function loginToServer(token: string, serverUrl?: string): Promise<void> {
  const origin = serverUrl?.trim() ? normalizeServerUrl(serverUrl) : webOrigin();
  if (!origin) throw new ServerLoginError('needsServer');
  const changingMobileEngine = isMobileApp && (connectionMode() === 'local' || savedServerUrl() !== origin);
  let response: Response;
  try {
    response = await fetch(`${origin}/api/auth/login`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ token: token.trim() }),
      signal: AbortSignal.timeout(10_000),
    });
  } catch {
    throw new ServerLoginError('unreachable', origin);
  }
  if (response.status === 401) throw new ServerLoginError('badToken');
  if (!response.ok) throw new ServerLoginError('failed', String(response.status));
  if (serverUrl?.trim()) localStorage.setItem(SERVER_URL_KEY, origin);
  localStorage.setItem(ACCESS_TOKEN_KEY, token.trim());
  localStorage.setItem('bridge_api_key', token.trim());
  resetBridgePort();
  if (changingMobileEngine) window.location.hash = '/';
}

export async function logoutFromServer(): Promise<void> {
  const origin = webOrigin();
  if (origin) {
    await fetch(`${origin}/api/auth/logout`, { method: 'POST' }).catch(() => undefined);
  }
  localStorage.removeItem(ACCESS_TOKEN_KEY);
  localStorage.removeItem('bridge_api_key');
  resetBridgePort();
}

/** Mobile app in local mode: show the server login screen. */
export function requestServerLogin(): void {
  emitLoginRequired(new LoginRequiredError(true, 'needsServer'));
}

/** Called by request helpers on HTTP 401 so a web client returns to the login screen. */
export function notifyUnauthorized(): void {
  if (connectionMode() !== 'web') return;
  localStorage.removeItem(ACCESS_TOKEN_KEY);
  apiReady = null;
  emitLoginRequired(new LoginRequiredError(isMobileApp || !!savedServerUrl(), 'expired'));
}
