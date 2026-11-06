import { describe, expect, it } from 'vitest';
import { expandSnapshotParts } from './snapshotParts';

describe('expandSnapshotParts', () => {
  it('restores shared prompts and tools into each snapshot', () => {
    const tools = [{ name: 'Bash', description: 'run', input_schema: {} }];
    const data = expandSnapshotParts({
      id: 'c',
      snapshotParts: { p0: 'system prompt', p1: tools },
      messages: [
        { role: 'user' },
        { role: 'assistant', requestSnapshots: [
          { id: 'a', systemPromptRef: 'p0', toolsRef: 'p1', systemMessages: [] },
          { id: 'b', systemPromptRef: 'p0', toolsRef: 'p1', systemMessages: [] },
        ] },
      ],
    });
    expect(data.snapshotParts).toBeUndefined();
    const [a, b] = (data.messages[1] as { requestSnapshots: Record<string, unknown>[] }).requestSnapshots;
    expect(a).toEqual({ id: 'a', systemPrompt: 'system prompt', tools, systemMessages: [] });
    expect(b.tools).toEqual(tools);
  });

  it('leaves responses without shared parts unchanged', () => {
    const data = { messages: [{ requestSnapshots: [{ id: 'a', systemPrompt: 'x' }] }] };
    expect(expandSnapshotParts(data)).toEqual({ messages: [{ requestSnapshots: [{ id: 'a', systemPrompt: 'x' }] }] });
  });
});
