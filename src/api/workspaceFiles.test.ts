import { beforeEach, expect, it, vi } from 'vitest';
const mocks = vi.hoisted(() => ({ request: vi.fn(), invoke: vi.fn() }));
vi.mock('./client', () => ({ request: mocks.request }));
vi.mock('../services/bridgeConnection', () => ({ hasNativeDesktop: () => true }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));
import { getWorkspaceFileCapabilities, getWorkspaceFileDiff, openWorkspaceFile, readWorkspaceFile, saveWorkspaceFileAs } from './workspaceFiles';
beforeEach(() => { vi.clearAllMocks(); mocks.request.mockResolvedValue({ json: async () => ({}) }); });
it('projects UI file tabs onto the strict wire reference for HTTP and native commands', async () => {
  const scope = { conversationId: 'session' };
  const file = { id: 'ui-tab', rootId: 'root', relativePath: '中文 space.txt', name: 'name', navigation: 2, scope };
  await readWorkspaceFile(scope, file);
  await getWorkspaceFileDiff(scope, file);
  for (const [, options] of mocks.request.mock.calls) expect(JSON.parse(options.body)).toEqual({ scope, rootId: 'root', relativePath: '中文 space.txt' });
  await getWorkspaceFileCapabilities(scope, file);
  await openWorkspaceFile(scope, file, 'terminal');
  await saveWorkspaceFileAs(scope, file);
  for (const [, options] of mocks.invoke.mock.calls) expect(options.file).toEqual({ rootId: 'root', relativePath: '中文 space.txt' });
});
