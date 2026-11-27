export interface ResourceAlertToast {
  id: number;
  text: string;
}

/**
 * Builds the next Desktop toast for a resource warning. Every accepted summary
 * gets a fresh id, even when the text repeats, so the auto-dismiss effect can
 * restart instead of keeping the first toast's timer. Blank summaries keep the
 * current toast and never show an empty warning.
 */
export function nextResourceAlertToast(
  current: ResourceAlertToast | null,
  summary: string | null | undefined,
): ResourceAlertToast | null {
  const trimmed = (summary || '').trim();
  if (!trimmed) return current;
  return {
    id: (current?.id ?? 0) + 1,
    text: `System resource warning: ${trimmed}`,
  };
}
