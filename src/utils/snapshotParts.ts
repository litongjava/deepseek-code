const SHARED_FIELDS = ['systemPrompt', 'systemMessages', 'tools'] as const;

/** Query that asks the server to send repeated snapshot fields once (see `expandSnapshotParts`). */
export const SNAPSHOT_PARTS_QUERY = 'snapshot_parts=1';

/**
 * Restores request snapshots sent with shared parts: the server moves each
 * distinct system prompt and tool list into `snapshotParts` and leaves
 * `<field>Ref` in the snapshot. Snapshots then look exactly as before.
 * Expanded snapshots share the same objects, which callers only read.
 */
export function expandSnapshotParts<T extends { messages?: unknown[]; snapshotParts?: Record<string, unknown> }>(data: T): T {
  const parts = data?.snapshotParts;
  if (!parts) return data;
  for (const message of (data.messages || []) as { requestSnapshots?: Record<string, unknown>[] }[]) {
    for (const snapshot of message?.requestSnapshots || []) {
      for (const field of SHARED_FIELDS) {
        const ref = snapshot[`${field}Ref`];
        if (typeof ref !== 'string') continue;
        if (ref in parts) snapshot[field] = parts[ref];
        delete snapshot[`${field}Ref`];
      }
    }
  }
  delete data.snapshotParts;
  return data;
}
