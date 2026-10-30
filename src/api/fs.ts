import { request } from './client';

export interface FsFileNode {
  name: string;
  path: string;
  is_dir: boolean;
  size: number;
  children?: FsFileNode[];
}

export interface FsTreeResponse {
  success: boolean;
  path: string;
  tree: FsFileNode[];
}

export interface FsReadResponse {
  success: boolean;
  path: string;
  content: string;
  /** Preview reads (`maxBytes`) only: file size on disk in bytes. */
  size?: number;
  /** Preview reads only: only the first `maxBytes` bytes were read. */
  truncated?: boolean;
  /** Preview reads only: NUL bytes or invalid UTF-8; `content` is empty. */
  binary?: boolean;
}

// Filesystem routes go through the Bridge, which requires the API key that
// `request` attaches. Failures keep the historical "<action>: <status>" message.
async function fsRequest<T>(path: string, init: RequestInit, action: string): Promise<T> {
  let res: Response;
  try {
    res = await request(path, init);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    const status = /^Request failed: (\d+)$/.exec(message)?.[1];
    throw new Error(`${action}: ${status ?? message}`);
  }
  return res.json();
}

/**
 * List a directory. `depth` (Bridge default 10, clamped to 1..10) is the number of
 * levels returned; directories at the last level have no `children` (not loaded).
 */
export async function fsTree(dirPath?: string, depth?: number): Promise<FsTreeResponse> {
  const params = new URLSearchParams();
  if (dirPath) params.set('path', dirPath);
  if (depth !== undefined) params.set('depth', String(depth));
  const query = params.toString();
  return fsRequest(`/filesystem/tree${query ? `?${query}` : ''}`, {}, 'Failed to get file tree');
}

/**
 * Read a file. With `maxBytes` the Bridge returns at most that many raw bytes plus
 * `size`, `truncated` and `binary`; without it the whole file is read as text.
 */
export async function fsRead(filePath: string, options: { maxBytes?: number } = {}): Promise<FsReadResponse> {
  const params = new URLSearchParams({ path: filePath });
  if (options.maxBytes !== undefined) params.set('max_bytes', String(options.maxBytes));
  return fsRequest(`/filesystem/read?${params}`, {}, 'Failed to read file');
}

export async function fsWrite(filePath: string, content: string): Promise<{ success: boolean; path: string }> {
  return fsRequest('/filesystem/write', {
    method: 'POST',
    body: JSON.stringify({ path: filePath, content }),
  }, 'Failed to write file');
}

export async function fsCreate(filePath: string, isDir = false): Promise<{ success: boolean; path: string }> {
  return fsRequest('/filesystem/create', {
    method: 'POST',
    body: JSON.stringify({ path: filePath, is_dir: isDir }),
  }, 'Failed to create');
}

export async function fsDelete(filePath: string): Promise<{ success: boolean; path: string }> {
  return fsRequest('/filesystem/delete', {
    method: 'DELETE',
    body: JSON.stringify({ path: filePath }),
  }, 'Failed to delete');
}
