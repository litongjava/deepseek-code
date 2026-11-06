import { notifyUnauthorized } from '../services/bridgeConnection';

// Account login is opt-in; local providers work without a gateway account.
export const loginEnabled = import.meta.env.VITE_ENABLE_LOGIN === 'true';

export function handleUnauthorized() {
  localStorage.removeItem('auth_token');
  localStorage.removeItem('user');
  // A dcd-server rejected the access token: show the server login screen.
  notifyUnauthorized();
  if (loginEnabled) {
    window.location.hash = '#/login';
    window.location.reload();
  }
}
