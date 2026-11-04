import { useEffect, useMemo, useState } from 'react';
import { useParams } from 'react-router-dom';
import { getWorkspaceRoots, type WorkspaceRoot, type WorkspaceScope } from '../../../api/workspaceFiles';
import { useProjectStore } from '../../../stores/useProjectStore';
import { workspaceScopeKey } from '../../../stores/useRightSidebarStore';

export function useCurrentWorkspaceScope(): WorkspaceScope {
  const { id } = useParams();
  const projectId = useProjectStore(state => state.pendingProjectId);
  return useMemo(() => id ? { conversationId: id } : projectId ? { projectId } : {}, [id, projectId]);
}

export function useWorkspaceRoots(scope: WorkspaceScope) {
  const [state, setState] = useState<{ key: string; roots: WorkspaceRoot[]; error?: string }>({ key: '', roots: [] });
  const [revision, refresh] = useState(0);
  const key = workspaceScopeKey(scope);
  useEffect(() => {
    const controller = new AbortController();
    if (!scope.conversationId && !scope.projectId) {
      setState({ key, roots: [] });
      return;
    }
    getWorkspaceRoots(scope, controller.signal).then(result => {
      if (!controller.signal.aborted) setState({ key, roots: result.roots });
    }).catch(error => {
      if (!controller.signal.aborted) setState({ key, roots: [], error: String(error.message || error) });
    });
    return () => controller.abort();
  }, [key, revision]);
  useEffect(() => {
    const changed = () => refresh(value => value + 1);
    window.addEventListener('projectsChanged', changed);
    window.addEventListener('conversationUpdated', changed);
    return () => { window.removeEventListener('projectsChanged', changed); window.removeEventListener('conversationUpdated', changed); };
  }, []);
  return { roots: state.key === key ? state.roots : [], error: state.key === key ? state.error : undefined, loading: state.key !== key, refresh: () => refresh(value => value + 1) };
}
