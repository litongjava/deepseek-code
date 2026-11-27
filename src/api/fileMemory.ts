import { request } from './client';
export interface MemoryScope { session_id?: string; project_id?: string }
export interface FileMemoryItem { id: string; name: string; description: string; type: 'user' | 'feedback' | 'project' | 'reference'; content: string; version: string }
export interface FileMemoryList { scope: string; directory: string; generation: number; items: FileMemoryItem[]; index: string; version: string; omitted: number; diagnostics: { file: string; message: string }[] }
async function json<T>(path: string, method: string, body: unknown): Promise<T> {
  return (await request(`/memory${path}`, { method, body: JSON.stringify(body) })).json();
}
export const fileMemoryApi = {
  list: (scope: MemoryScope, query = '') => json<FileMemoryList>('/search', 'POST', { scope, query }),
  save: (scope: MemoryScope, name: string, content: string, version?: string) => json('/ingest', 'POST', { scope, name, content, version }),
  remove: (scope: MemoryScope, item: FileMemoryItem) => json(`/items/${encodeURIComponent(item.id)}`, 'DELETE', { scope, version: item.version }),
};
