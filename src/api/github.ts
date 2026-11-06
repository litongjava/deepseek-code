import { request } from './client';

// GitHub connector routes served by the Bridge (`/api/github/*`).

export async function getGithubStatus() {
  return (await request('/github/status')).json();
}

export async function getGithubAuthUrl(): Promise<{ url: string; state: string }> {
  return (await request('/github/auth-url')).json();
}

/** Connects with a personal access token; works without an OAuth app secret. */
export async function connectGithubToken(token: string) {
  return (await request('/github/token', { method: 'POST', body: JSON.stringify({ token }) })).json();
}

export async function disconnectGithub() {
  return (await request('/github/disconnect', { method: 'POST' })).json();
}

export async function getGithubRepos(page = 1) {
  return (await request(`/github/repos?page=${page}`)).json();
}

export async function getGithubTree(owner: string, repo: string, ref = '') {
  const qs = ref ? `?ref=${encodeURIComponent(ref)}` : '';
  return (await request(`/github/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/tree${qs}`)).json();
}

export async function getGithubContents(owner: string, repo: string, path = '', ref = '') {
  const params = new URLSearchParams();
  if (path) params.set('path', path);
  if (ref) params.set('ref', ref);
  const qs = params.toString();
  return (await request(`/github/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/contents${qs ? '?' + qs : ''}`)).json();
}

export async function materializeGithub(
  conversationId: string,
  repoFullName: string,
  ref: string,
  selections: Array<{ path: string; isFolder: boolean }>
): Promise<{ ok: boolean; repoFullName: string; ref: string; rootDir: string; fileCount: number; skipped: number }> {
  const res = await request('/github/materialize', {
    method: 'POST',
    body: JSON.stringify({ conversationId, repoFullName, ref, selections }),
  });
  return res.json();
}
