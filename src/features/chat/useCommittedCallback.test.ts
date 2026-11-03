import { renderHook } from '@testing-library/react';
import { describe, expect, it } from 'vitest';
import { useCommittedCallback } from './useCommittedCallback';

describe('useCommittedCallback', () => {
  it('keeps identity while observing current committed state', () => {
    const hook = renderHook(({ value }) => useCommittedCallback(() => value), { initialProps: { value: 'first' } });
    const callback = hook.result.current;
    hook.rerender({ value: 'second' });
    expect(hook.result.current).toBe(callback);
    expect(callback()).toBe('second');
  });
});
