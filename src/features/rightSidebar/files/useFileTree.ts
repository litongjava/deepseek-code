import { useCallback, useEffect, useRef, useState } from 'react';
import { getFileSystemTree, type FsFileNode } from '../../../api';

export type DirState =
  | { status: 'loading'; entries?: FsFileNode[] }
  | { status: 'loaded'; entries: FsFileNode[] }
  | { status: 'error'; message: string };

/**
 * Lazily loaded directory listing rooted at `root`. Each directory is fetched with
 * `depth=1` when first expanded; nothing is expanded automatically. `refresh`
 * reloads the root and every expanded directory and forgets collapsed ones.
 */
export function useFileTree(root: string | null) {
  const [dirs, setDirs] = useState<Record<string, DirState>>({});
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
  const dirsRef = useRef(dirs);
  dirsRef.current = dirs;
  const expandedRef = useRef(expanded);
  expandedRef.current = expanded;
  // Responses for a previous root are dropped.
  const generation = useRef(0);

  const loadDir = useCallback((path: string) => {
    const gen = generation.current;
    setDirs(prev => {
      const current = prev[path];
      // Keep showing loaded entries while they are reloaded.
      return { ...prev, [path]: { status: 'loading', entries: current?.status === 'loaded' ? current.entries : undefined } };
    });
    getFileSystemTree(path, 1)
      .then(res => {
        if (gen !== generation.current) return;
        setDirs(prev => ({ ...prev, [path]: { status: 'loaded', entries: Array.isArray(res.tree) ? res.tree : [] } }));
      })
      .catch((error: unknown) => {
        if (gen !== generation.current) return;
        setDirs(prev => ({ ...prev, [path]: { status: 'error', message: error instanceof Error ? error.message : String(error) } }));
      });
  }, []);

  useEffect(() => {
    generation.current += 1;
    setDirs({});
    setExpanded(new Set());
    if (root) loadDir(root);
  }, [root, loadDir]);

  const setDirExpanded = useCallback((path: string, open: boolean) => {
    if (open === expandedRef.current.has(path)) return;
    setExpanded(prev => {
      const next = new Set(prev);
      if (open) next.add(path);
      else next.delete(path);
      return next;
    });
    const state = dirsRef.current[path];
    if (open && (!state || state.status === 'error')) loadDir(path);
  }, [loadDir]);

  const toggle = useCallback((path: string) => {
    setDirExpanded(path, !expandedRef.current.has(path));
  }, [setDirExpanded]);

  const refresh = useCallback(() => {
    if (!root) return;
    const keep = [root, ...expandedRef.current];
    setDirs(prev => Object.fromEntries(keep.filter(path => prev[path]).map(path => [path, prev[path]])));
    for (const path of keep) loadDir(path);
  }, [root, loadDir]);

  return { dirs, expanded, setDirExpanded, toggle, refresh };
}
