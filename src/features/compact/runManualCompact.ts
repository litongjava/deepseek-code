import { compactConversation, getContextSize } from '../../api';
import { useChatStore } from '../../stores/useChatStore';
import { useProjectStore } from '../../stores/useProjectStore';

export async function runManualCompact(
  id: string,
  instruction: string,
  loadConversation: (id: string, isCurrent: () => boolean) => Promise<void>,
  isCurrent: () => boolean,
) {
  const { compactStatus, setCompactStatus } = useChatStore.getState();
  if (!isCurrent() || compactStatus.state === 'compacting') return;
  setCompactStatus({ state: 'compacting' });

  let result;
  try {
    result = await compactConversation(id, instruction.trim() || undefined);
  } catch (error) {
    if (isCurrent()) {
      setCompactStatus({ state: 'error', message: error instanceof Error ? error.message : 'Compaction failed' });
    }
    return;
  }
  if (!isCurrent()) return;
  // A refresh failure cannot undo the committed boundary.
  const [, context] = await Promise.allSettled([loadConversation(id, isCurrent), getContextSize(id)]);
  if (!isCurrent()) return;
  setCompactStatus({
    state: 'done',
    message: `Compacted ${result.messagesCompacted} messages, saved ~${result.tokensSaved} tokens`,
  });
  if (context.status === 'fulfilled') {
    useProjectStore.getState().setContextInfo(context.value);
  }
}
