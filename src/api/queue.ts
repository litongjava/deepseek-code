import { request } from './client';
import type { UserContent } from '../utils/userContent';

export interface QueueItem {
  id: number;
  sessionId: string;
  requestId: string;
  revision: number;
  content: UserContent;
  mode: 'next_turn' | 'steer';
  state: 'queued' | 'claimed' | 'handoff' | 'cancelled';
  handoffId?: string | null;
}
export interface QueueSnapshot { items: QueueItem[]; paused: boolean }
export interface QueueSubmission { requestId: string; content: UserContent; mode: QueueItem['mode'] }
const path = (session: string) => `/conversations/${encodeURIComponent(session)}/queue`;
export const queueAPI = {
  async list(session: string): Promise<QueueSnapshot> {
    return (await request(path(session))).json();
  },
  async enqueue(session: string, submission: QueueSubmission): Promise<QueueItem> {
    return (await request(path(session), { method: 'POST', body: JSON.stringify(submission) })).json();
  },
  async mutate(session: string, item: QueueItem, action: 'edit' | 'delete' | 'steer', text?: string): Promise<QueueItem> {
    return (await request(`${path(session)}/${item.id}${action === 'steer' ? '/steer' : ''}`, {
      method: action === 'edit' ? 'PATCH' : action === 'delete' ? 'DELETE' : 'POST',
      body: JSON.stringify({ revision: item.revision, ...(action === 'edit' ? { text } : {}) }),
    })).json();
  },
  async resume(session: string): Promise<QueueSnapshot> {
    return (await request(`${path(session)}/resume`, { method: 'POST', body: '{}' })).json();
  },
};
