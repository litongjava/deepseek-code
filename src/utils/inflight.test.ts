import { describe, expect, it, vi } from 'vitest';
import { shareInFlight } from './inflight';

describe('shareInFlight', () => {
  it('runs one read for concurrent callers and a new read after it settles', async () => {
    const read = vi.fn().mockResolvedValue(['a']);
    const [first, second] = await Promise.all([shareInFlight('k', read), shareInFlight('k', read)]);
    expect(read).toHaveBeenCalledTimes(1);
    expect(first).toBe(second);
    await shareInFlight('k', read);
    expect(read).toHaveBeenCalledTimes(2);
  });

  it('releases the key after a failure', async () => {
    const read = vi.fn().mockRejectedValueOnce(new Error('offline')).mockResolvedValue(1);
    await expect(shareInFlight('f', read)).rejects.toThrow('offline');
    await expect(shareInFlight('f', read)).resolves.toBe(1);
  });

  it('keeps different keys independent', async () => {
    const a = vi.fn().mockResolvedValue('a');
    const b = vi.fn().mockResolvedValue('b');
    await Promise.all([shareInFlight('a', a), shareInFlight('b', b)]);
    expect(a).toHaveBeenCalledTimes(1);
    expect(b).toHaveBeenCalledTimes(1);
  });
});
