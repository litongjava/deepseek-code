// Account login is opt-in; local providers work without a gateway account.
export const loginEnabled = import.meta.env.VITE_ENABLE_LOGIN === 'true';

export function handleUnauthorized() {
  localStorage.removeItem('auth_token');
  localStorage.removeItem('user');
  if (loginEnabled) {
    window.location.hash = '#/login';
    window.location.reload();
  }
}
