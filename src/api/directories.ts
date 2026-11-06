import { request } from './client';

export interface DirectoryListing {
  path: string;
  parent: string | null;
  home: string | null;
  roots: string[];
  directories: { name: string; path: string }[];
  truncated: number;
}

/** Subdirectories of `path` on the server (its home directory when omitted). */
export async function listDirectories(path?: string, hidden = false): Promise<DirectoryListing> {
  const query = new URLSearchParams();
  if (path) query.set('path', path);
  if (hidden) query.set('hidden', 'true');
  const suffix = query.toString();
  return (await request(`/filesystem/directories${suffix ? `?${suffix}` : ''}`)).json();
}
