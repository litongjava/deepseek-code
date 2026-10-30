import { useEffect, useState } from 'react';
import { useParams } from 'react-router-dom';
import { getConversation, getProject } from '../../../api';
import { useProjectStore } from '../../../stores/useProjectStore';

export type WorkspaceFolder =
  | { status: 'loading' }
  | { status: 'none' }
  | { status: 'ready'; path: string };

interface ConversationWorkspace {
  id: string;
  workspacePath: string | null;
  projectId: string | null;
}

type ProjectFolder = { id: string; path: string | null };

/**
 * Folder of a project. `useProjectStore.projectList` is only filled when the
 * composer's plus menu opens, so a project missing there is fetched directly.
 * Returns undefined while loading and null when the project has no folder.
 */
function useProjectFolder(projectId: string | null): string | null | undefined {
  const listed = useProjectStore(s => (projectId ? s.projectList.find(p => p.id === projectId) : undefined));
  const listedPath = listed?.workspace_path || listed?.path || null;
  const [fetched, setFetched] = useState<ProjectFolder | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    const handleChange = () => setReloadKey(key => key + 1);
    window.addEventListener('projectsChanged', handleChange);
    return () => window.removeEventListener('projectsChanged', handleChange);
  }, []);

  useEffect(() => {
    if (!projectId || listedPath) return;
    let cancelled = false;
    getProject(projectId)
      .then((project: { workspace_path?: string | null; folders?: Array<{ path: string }> } | null) => {
        const path = project?.workspace_path || project?.folders?.[0]?.path || null;
        if (!cancelled) setFetched({ id: projectId, path });
      })
      .catch(() => {
        if (!cancelled) setFetched({ id: projectId, path: null });
      });
    return () => { cancelled = true; };
  }, [projectId, listedPath, reloadKey]);

  if (!projectId) return null;
  if (listedPath) return listedPath;
  return fetched?.id === projectId ? fetched.path : undefined;
}

/**
 * The folder the Files tab shows: the open conversation's workspace (project chats
 * fall back to their project's folder) or, on Home, the project selected in the
 * composer's project picker (`pendingProjectId`). Reloads when the conversation
 * reports an update or is attached to another project (`currentProjectId`).
 */
export function useWorkspaceFolder(): WorkspaceFolder {
  const { id } = useParams();
  const pendingProjectId = useProjectStore(s => s.pendingProjectId);
  const currentProjectId = useProjectStore(s => s.currentProjectId);
  const [conversation, setConversation] = useState<ConversationWorkspace | null>(null);
  const [reloadKey, setReloadKey] = useState(0);

  useEffect(() => {
    if (!id) return;
    const handleUpdate = (event: Event) => {
      const detail = (event as CustomEvent<{ id?: string } | undefined>).detail;
      if (!detail?.id || detail.id === id) setReloadKey(key => key + 1);
    };
    window.addEventListener('conversationUpdated', handleUpdate);
    return () => window.removeEventListener('conversationUpdated', handleUpdate);
  }, [id]);

  useEffect(() => {
    if (!id) return;
    let cancelled = false;
    getConversation(id)
      .then(data => {
        if (!cancelled) setConversation({ id, workspacePath: data.workspace_path || null, projectId: data.project_id || null });
      })
      .catch(() => {
        if (!cancelled) setConversation({ id, workspacePath: null, projectId: null });
      });
    return () => { cancelled = true; };
    // Attaching the open chat to a project changes currentProjectId and its workspace.
  }, [id, reloadKey, currentProjectId]);

  const conversationLoaded = !!id && conversation?.id === id;
  const projectId = !id ? pendingProjectId
    : conversationLoaded && !conversation.workspacePath ? (conversation.projectId || currentProjectId)
      : null;
  const projectFolder = useProjectFolder(projectId);

  if (id && !conversationLoaded) return { status: 'loading' };
  const path = (conversationLoaded ? conversation.workspacePath : null) || projectFolder;
  if (path) return { status: 'ready', path };
  return projectFolder === undefined ? { status: 'loading' } : { status: 'none' };
}
