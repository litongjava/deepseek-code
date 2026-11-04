export interface TurnRecord {
  turnId: string;
  startedAt: string;
  endedAt: string | null;
  elapsedMs: number | null;
  status: 'running' | 'completed' | 'failed' | 'cancelled';
}

export function readTurn(value: unknown): TurnRecord | undefined {
  const turn = value as TurnRecord | null;
  if (!turn || typeof turn.turnId !== 'string' || !turn.turnId || typeof turn.startedAt !== 'string'
    || !['running', 'completed', 'failed', 'cancelled'].includes(turn.status)) return undefined;
  if (turn.status !== 'running' && (typeof turn.elapsedMs !== 'number' || !Number.isFinite(turn.elapsedMs) || turn.elapsedMs < 0 || typeof turn.endedAt !== 'string')) return undefined;
  return turn;
}

export function turnLabel(value: unknown): string | null {
  const turn = readTurn(value);
  if (!turn) return null;
  if (turn.status === 'running') return 'Working…';
  const seconds = Math.floor(turn.elapsedMs! / 1000);
  const duration = seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, '0')}s`;
  return `Worked for ${duration}${turn.status === 'completed' ? '' : ` · ${turn.status === 'failed' ? 'Failed' : 'Cancelled'}`}`;
}

export function applyTurnTiming(messages: any[], value: unknown, assistantId?: string): any[] {
  const turn = readTurn(value);
  if (!turn) return messages;
  let matched = false;
  const updated = messages.map(message => {
    if (message.role !== 'assistant' || message.turn?.turnId !== turn.turnId) return message;
    matched = true;
    return { ...message, turn };
  });
  if (!matched) {
    const index = updated.length - 1;
    if (updated[index]?.role === 'assistant' && !updated[index].turn) updated[index] = { ...updated[index], id: assistantId || updated[index].id, turn };
    else updated.push({ role: 'assistant', content: '', turn });
  }
  return updated;
}
