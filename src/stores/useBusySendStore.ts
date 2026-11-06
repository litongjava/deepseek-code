import { create } from 'zustand';
import { queueAPI, type QueueItem, type QueueSubmission, type QueueSnapshot } from '../api/queue';
import { describeError } from '../utils/networkError';

export interface PendingSubmission extends QueueSubmission { status: 'sending' | 'failed'; error?: string }
interface SessionQueue {
  items: QueueItem[];
  pending: PendingSubmission[];
  paused: boolean;
  error?: string;
  busy: number[];
}
const empty = (): SessionQueue => ({ items: [], pending: [], paused: false, busy: [] });
const storageKey = 'durable-queue-outbox-v1';
function restore(): Record<string, SessionQueue> {
  try {
    const saved = JSON.parse(localStorage.getItem(storageKey) || '{}');
    return Object.fromEntries(Object.entries(saved).map(([id, pending]) => [id, {
      ...empty(), pending: (pending as PendingSubmission[]).map(item => ({ ...item, status: 'failed', error: 'Submission not confirmed. Retry safely.' })),
    }]));
  } catch { return {}; }
}
function persist(queues: Record<string, SessionQueue>) {
  localStorage.setItem(storageKey, JSON.stringify(Object.fromEntries(Object.entries(queues).filter(([, q]) => q.pending.length).map(([id, q]) => [id, q.pending]))));
}
const versions: Record<string, number> = {};
const errorText = describeError;
interface QueueStore {
  queues: Record<string, SessionQueue>;
  enqueue: (session: string, content: QueueSubmission['content'], mode: QueueItem['mode']) => Promise<void>;
  retry: (session: string, requestId: string) => Promise<void>;
  refresh: (session: string) => Promise<void>;
  accept: (session: string, item: QueueItem) => void;
  mutate: (session: string, item: QueueItem, action: 'edit' | 'delete' | 'steer', text?: string) => Promise<boolean>;
  resume: (session: string) => Promise<void>;
}
export const useBusySendStore = create<QueueStore>((set, get) => {
  const update = (session: string, change: (q: SessionQueue) => SessionQueue) => {
    set(state => ({ queues: { ...state.queues, [session]: change(state.queues[session] || empty()) } }));
  };
  const save = () => { try { persist(get().queues); } catch { /* Keep the in-memory outbox if cleanup storage fails. */ } };
  const snapshot = (session: string, value: QueueSnapshot) => {
    update(session, q => {
      const items = new Map(q.items.map(item => [item.id, item]));
      for (const item of value.items) {
        if (item.sessionId === session && (!items.has(item.id) || items.get(item.id)!.revision <= item.revision)) items.set(item.id, item);
      }
      const known = new Set([...items.values()].map(item => item.requestId));
      return { ...q, items: [...items.values()], pending: q.pending.filter(item => !known.has(item.requestId)), paused: value.paused, error: undefined };
    });
    save();
  };
  return {
    queues: restore(),
    accept: (session, item) => {
      if (item.sessionId !== session) return;
      versions[session] = (versions[session] || 0) + 1;
      snapshot(session, { items: [item], paused: get().queues[session]?.paused || false });
    },
    enqueue: async (session, content, mode) => {
      const submission: PendingSubmission = { requestId: crypto.randomUUID(), content, mode, status: 'failed' };
      const queues = { ...get().queues, [session]: { ...(get().queues[session] || empty()), pending: [...(get().queues[session]?.pending || []), submission] } };
      // Persist before the composer releases ownership, including image bytes.
      persist(queues);
      set({ queues });
      await get().retry(session, submission.requestId);
    },
    retry: async (session, requestId) => {
      const item = get().queues[session]?.pending.find(p => p.requestId === requestId);
      if (!item || item.status === 'sending') return;
      update(session, q => ({ ...q, pending: q.pending.map(p => p.requestId === requestId ? { ...p, status: 'sending', error: undefined } : p) }));
      try {
        const ack = await queueAPI.enqueue(session, { requestId, content: item.content, mode: item.mode });
        get().accept(session, ack);
      } catch (error) {
        update(session, q => ({ ...q, pending: q.pending.map(p => p.requestId === requestId ? { ...p, status: 'failed', error: errorText(error) } : p) }));
      }
      save();
    },
    refresh: async session => {
      const version = versions[session] = (versions[session] || 0) + 1;
      try {
        const value = await queueAPI.list(session);
        if (versions[session] === version) snapshot(session, value);
      } catch (error) {
        if (versions[session] === version) update(session, q => ({ ...q, error: errorText(error) }));
      }
    },
    mutate: async (session, item, action, text) => {
      const q = get().queues[session];
      if (!q || q.busy.includes(item.id) || item.state !== 'queued') return false;
      versions[session] = (versions[session] || 0) + 1;
      update(session, q => ({ ...q, busy: [...q.busy, item.id], error: undefined }));
      try {
        get().accept(session, await queueAPI.mutate(session, item, action, text));
        return true;
      } catch (error) {
        await get().refresh(session);
        update(session, q => ({ ...q, error: errorText(error) }));
        return false;
      } finally {
        update(session, q => ({ ...q, busy: q.busy.filter(id => id !== item.id) }));
      }
    },
    resume: async session => {
      const version = versions[session] = (versions[session] || 0) + 1;
      try {
        const value = await queueAPI.resume(session);
        if (versions[session] === version) snapshot(session, value);
        else await get().refresh(session);
      } catch (error) {
        if (versions[session] === version) update(session, q => ({ ...q, error: errorText(error) }));
      }
    },
  };
});
