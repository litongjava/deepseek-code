import { request } from './client';
import { mapHistoryMessage } from '../api';

export interface SubagentInvocation {
  parent_session_id: string;
  parent_message_id: string;
  tool_use_id: string;
  child_session_id: string;
  attempt_id: string;
  mode: string;
  status: string;
  description: string;
}
export interface SubagentSnapshot {
  invocation: SubagentInvocation;
  messages: any[];
  next_offset: number | null;
  cursor: string;
  status: string;
  parent_exists: boolean;
}
export const isSubagentRunning = (status: string) => ['running', 'pending', 'queued', 'starting', 'waiting', 'waiting_for_input'].includes(status);

export async function listSubagents(parentId: string, toolId: string, signal?: AbortSignal): Promise<SubagentInvocation[]> {
  const response = await request(`/conversations/${encodeURIComponent(parentId)}/subagents?tool_use_id=${encodeURIComponent(toolId)}`, { signal });
  const data = await response.json();
  return data.invocations.filter((item: SubagentInvocation) => item.parent_session_id === parentId && item.tool_use_id === toolId);
}

/** A complete authoritative snapshot, never a preview or a merge with parent chat state. */
export async function getSubagentSnapshot(parentId: string, childId: string, signal: AbortSignal): Promise<SubagentSnapshot> {
  let offset = 0;
  let first: SubagentSnapshot | undefined;
  const messages = new Map<string, any>();
  do {
    signal.throwIfAborted();
    const response = await request(`/conversations/${encodeURIComponent(parentId)}/subagents/${encodeURIComponent(childId)}?offset=${offset}&limit=100`, { signal });
    const page: SubagentSnapshot = await response.json();
    signal.throwIfAborted();
    if (page.invocation.parent_session_id !== parentId || page.invocation.child_session_id !== childId) throw new Error('Subagent identity mismatch');
    if (first && page.cursor !== first.cursor) throw new Error('Transcript changed while loading. Retrying…');
    first ??= page;
    for (const message of page.messages) {
      if (!message.id) throw new Error('Transcript message identity unavailable');
      messages.set(message.id, mapHistoryMessage(message));
    }
    if (page.next_offset === null) return { ...page, messages: [...messages.values()] };
    if (!Number.isInteger(page.next_offset) || page.next_offset <= offset) throw new Error('Invalid transcript page');
    offset = page.next_offset;
  } while (true);
}
