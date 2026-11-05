import { useEffect, useState } from 'react';
import { getSubagentSnapshot, isSubagentRunning, type SubagentSnapshot } from '../../api/subagents';

export function useSubagentTranscript(parentId: string, childId: string, active: boolean) {
  const [snapshot, setSnapshot] = useState<SubagentSnapshot | null>(null);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const [revision, setRevision] = useState(0);
  useEffect(() => { setSnapshot(null); setError(''); }, [parentId, childId]);
  useEffect(() => {
    if (!active) return;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      setLoading(true);
      let again = true;
      try {
        const next = await getSubagentSnapshot(parentId, childId, controller.signal);
        if (controller.signal.aborted) return;
        setSnapshot(next); setError('');
        again = isSubagentRunning(next.status);
      } catch (err) {
        if (controller.signal.aborted) return;
        setError(err instanceof Error ? err.message : String(err));
      } finally {
        if (!controller.signal.aborted) {
          setLoading(false);
          if (again) timer = setTimeout(poll, 2000);
        }
      }
    };
    void poll();
    return () => { controller.abort(); clearTimeout(timer); };
  }, [parentId, childId, active, revision]);
  return { snapshot, error, loading, retry: () => setRevision(value => value + 1) };
}
