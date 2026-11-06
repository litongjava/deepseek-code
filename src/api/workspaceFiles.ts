import { invokeDesktop as invoke } from '../platform/native';
import { request } from './client';
import { hasNativeDesktop } from '../services/bridgeConnection';

export interface WorkspaceScope { conversationId?: string; projectId?: string }
export interface WorkspaceFileRef { rootId: string; relativePath: string }
export interface WorkspaceRoot { rootId: string; name: string; path: string }
export interface WorkspaceTreeEntry extends WorkspaceFileRef { name: string; kind: 'file' | 'directory'; size: number | null }
export interface WorkspaceTree { entries: WorkspaceTreeEntry[]; truncated: boolean }
export interface WorkspaceFile extends WorkspaceFileRef { path: string; content: string | null; size: number; binary: boolean; truncated: boolean; notice: string | null }
export interface WorkspaceSearchMatch extends WorkspaceFileRef { line: number; startColumn: number; endColumn: number; preview: string }
export interface WorkspaceSearch { matches: WorkspaceSearchMatch[]; truncated: boolean; skippedFiles: number; scannedFiles: number; notice: string | null }
export interface WorkspaceFileDiff { patch: string; notice: string | null; baselineOid: string | null; baseline: 'head' | 'empty-tree' | 'none'; status: 'modified' | 'unchanged' | 'untracked' | 'not-git' }
export interface WorkspaceFileCapabilities { reveal: boolean; defaultApp: boolean; terminal: boolean; saveAs: boolean; wslDistributions: string[]; reason: string | null }
export type WorkspaceFileAction = 'reveal' | 'default-app' | 'terminal' | 'wsl-terminal';
export interface WorkspaceSaveResult { status: 'saved' | 'cancelled' | 'same-file' }

const fileReference = ({ rootId, relativePath }: WorkspaceFileRef): WorkspaceFileRef => ({ rootId, relativePath });

async function workspaceRequest<T>(operation: string, body: object, signal?: AbortSignal): Promise<T> {
  return (await request(`/workspace-files/${operation}`, { method: 'POST', body: JSON.stringify(body), signal })).json();
}

export function getWorkspaceRoots(scope: WorkspaceScope, signal?: AbortSignal): Promise<{ roots: WorkspaceRoot[] }> {
  return workspaceRequest('roots', { scope }, signal);
}
export function listWorkspaceTree(scope: WorkspaceScope, rootId: string, relativePath = '', signal?: AbortSignal): Promise<WorkspaceTree> {
  return workspaceRequest('tree', { scope, rootId, relativePath }, signal);
}
export function readWorkspaceFile(scope: WorkspaceScope, file: WorkspaceFileRef, signal?: AbortSignal): Promise<WorkspaceFile> {
  return workspaceRequest('read', { scope, ...fileReference(file) }, signal);
}
export function searchWorkspaceFiles(scope: WorkspaceScope, query: string, signal?: AbortSignal): Promise<WorkspaceSearch> {
  return workspaceRequest('search', { scope, query }, signal);
}
export function getWorkspaceFileDiff(scope: WorkspaceScope, file: WorkspaceFileRef, signal?: AbortSignal): Promise<WorkspaceFileDiff> {
  return workspaceRequest('diff', { scope, ...fileReference(file) }, signal);
}
export async function getWorkspaceFileCapabilities(scope: WorkspaceScope, file: WorkspaceFileRef): Promise<WorkspaceFileCapabilities> {
  if (!hasNativeDesktop()) return { reveal: false, defaultApp: false, terminal: false, saveAs: false, wslDistributions: [], reason: 'Native file actions require the Desktop application.' };
  return invoke('workspace_file_capabilities', { scope, file: fileReference(file) });
}
export async function openWorkspaceFile(scope: WorkspaceScope, file: WorkspaceFileRef, action: WorkspaceFileAction, distribution?: string): Promise<void> {
  if (!hasNativeDesktop()) throw new Error('Native file actions require the Desktop application.');
  await invoke('workspace_file_action', { scope, file: fileReference(file), action, distribution });
}
export async function saveWorkspaceFileAs(scope: WorkspaceScope, file: WorkspaceFileRef): Promise<WorkspaceSaveResult> {
  if (!hasNativeDesktop()) throw new Error('Save as requires the Desktop application.');
  return invoke('workspace_file_save_as', { scope, file: fileReference(file) });
}
