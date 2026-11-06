/**
 * Optional hosted account service (sign-in, plans, usage quotas).
 *
 * Everything else, including GitHub, sync and the model gateway, is served by
 * the Bridge. Builds that offer hosted accounts set VITE_ACCOUNT_SERVICE_URL;
 * without it, account calls fail immediately instead of probing a local port.
 */
export const ACCOUNT_SERVICE_URL = String(import.meta.env.VITE_ACCOUNT_SERVICE_URL || '').trim().replace(/\/+$/, '');
export const ACCOUNT_API = ACCOUNT_SERVICE_URL ? `${ACCOUNT_SERVICE_URL}/api` : '';

/** Path prefixes owned by the account service rather than the Bridge. */
export const ACCOUNT_ROUTES = [
  '/auth/', '/user/', '/payment/', '/redemption/',
  '/artifacts/', '/code/', '/multiagent/',
  '/conversations/export', '/conversations/sso',
];

export function isAccountRoute(path: string): boolean {
  return ACCOUNT_ROUTES.some(prefix => path.startsWith(prefix));
}

export class AccountServiceUnavailableError extends Error {
  constructor() {
    super('No account service is configured');
    this.name = 'AccountServiceUnavailableError';
  }
}

/** Absolute URL of an account-service API path such as `/user/profile`. */
export function accountUrl(path: string): string {
  if (!ACCOUNT_API) throw new AccountServiceUnavailableError();
  return `${ACCOUNT_API}${path}`;
}
