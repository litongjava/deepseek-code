import { create } from 'zustand';

export type ConversationView = 'chat' | 'reading' | 'trajectory';

export const useConversationViewStore = create<{
  view: ConversationView;
  setView: (view: ConversationView) => void;
}>((set) => ({
  view: 'chat',
  setView: (view) => set({ view }),
}));
