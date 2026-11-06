import { beforeEach, describe, expect, it, vi } from "vitest";
import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { MemoryRouter, useLocation } from "react-router-dom";
import SidebarProjects from "./SidebarProjects";
import { SidebarChat, useSidebarPreferences } from "./sidebarModel";
import { useProjectStore } from "../../stores/useProjectStore";
import { useWorkspaceModeStore } from "../../stores/useWorkspaceModeStore";
import {
  createProject,
  trustProjectFolders,
  deleteProject,
  getConversation,
  getProjects,
  updateConversation,
  updateProject,
} from "../../api";

vi.mock("../../api/directories", () => ({
  listDirectories: vi.fn(async (path?: string) => ({
    path: path || "C:\\Users\\dev", parent: null, home: "C:\\Users\\dev", roots: ["C:\\"], directories: [], truncated: 0,
  })),
}));
vi.mock("../../api", () => ({
  getProjects: vi.fn(),
  updateConversation: vi.fn(),
  updateProject: vi.fn(),
  deleteProject: vi.fn(),
  getConversation: vi.fn(),
  createProjectConversation: vi.fn(),
  createProject: vi.fn(),
  trustProjectFolders: vi.fn(),
}));
vi.mock("../../utils/tauriAPI", () => ({
  tauriAPI: { isTauri: false, openFolder: vi.fn() },
}));
const project = {
  id: "p1",
  name: "Desktop",
  workspace_path: "C:\\work\\desktop",
  folders: [{ path: "C:\\work\\desktop", trusted: true }],
  description: "",
  instructions: "",
  created_at: "",
  updated_at: "",
  is_archived: 0,
};
const other = { ...project, id: "p2", name: "Browser" };
const chats: SidebarChat[] = [
  {
    id: "a",
    title: "Pinned task",
    project_id: "p1",
    workspace_mode: "code",
    pinned: true,
  },
  { id: "b", title: "Project task", project_id: "p1", workspace_mode: "code" },
  { id: "c", title: "Loose task", workspace_mode: "code" },
  {
    id: "d",
    title: "Archived task",
    project_id: "p1",
    workspace_mode: "code",
    archived: true,
  },
  {
    id: "e",
    title: "Chat mode task",
    project_id: "p1",
    workspace_mode: "chat",
  },
];
beforeEach(() => {
  vi.resetAllMocks();
  localStorage.clear();
  useProjectStore.setState({ currentProjectId: null, pendingProjectId: "p1" });
  useWorkspaceModeStore.setState({
    mode: "code",
    recentsCollapsed: { chat: false, work: false, study: false, code: false },
  });
  useSidebarPreferences.setState({
    pinnedProjects: [],
    unreadChats: [],
    collapsedProjects: [],
    pinnedCollapsed: false,
    projectsCollapsed: false,
  });
  vi.mocked(getProjects).mockResolvedValue([project, other]);
  vi.mocked(updateConversation).mockResolvedValue({});
});
const setup = async (initial = chats) => {
  const open = vi.fn();
  const result = render(
    <MemoryRouter>
      <LocationState />
      <SidebarProjects
        chats={initial}
        streamingIds={new Set()}
        onOpenChat={open}
      />
    </MemoryRouter>,
  );
  await screen.findByRole("button", { name: "Desktop" });
  return { ...result, open };
};
function LocationState() {
  const location = useLocation();
  return <output data-testid="location">{JSON.stringify({ path: location.pathname, state: location.state })}</output>;
}
const chatMenu = (title = "Project task") =>
  fireEvent.click(
    within(screen.getByRole("region", { name: "Recent chats" })).getByRole("button", { name: `Chat options for ${title}` }),
  );
const projectMenu = () =>
  fireEvent.click(
    screen.getByRole("button", { name: "Project options for Desktop" }),
  );

describe("project sidebar", () => {
  it("opens Chat with the created project after folder trust succeeds", async () => {
    vi.mocked(createProject).mockResolvedValue({ ...project, folders: [{ path: project.workspace_path, trusted: false }] });
    vi.mocked(trustProjectFolders).mockResolvedValue(project);
    await setup();
    fireEvent.click(screen.getByRole('button', { name: 'Create project' }));
    const dialog = screen.getByRole('dialog', { name: 'Create project' });
    fireEvent.change(within(dialog).getByLabelText('Folder path'), { target: { value: project.workspace_path } });
    fireEvent.click(within(dialog).getByRole('button', { name: 'Add' }));
    fireEvent.click(within(dialog).getByRole('button', { name: 'Create project' }));
    const trust = await screen.findByRole('button', { name: 'Trust folder' });
    expect(screen.getByTestId('location')).not.toHaveTextContent('createdProjectId');
    fireEvent.click(trust);
    await waitFor(() => expect(screen.getByTestId('location')).toHaveTextContent('"state":{"createdProjectId":"p1"}'));
    expect(screen.getByTestId('location')).toHaveTextContent('"path":"/"');
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
  });
  it.each(["chat", "work", "study", "code"] as const)("keeps empty Recents visible in %s", async mode => {
    useWorkspaceModeStore.setState({ mode });
    await setup([]);
    const recents = screen.getByRole("region", { name: "Recent chats" });
    expect(within(recents).getByRole("button", { name: "Recents" })).toHaveAttribute("aria-expanded", "true");
    expect(within(recents).getByText("No recent chats")).toBeInTheDocument();
    fireEvent.click(within(recents).getByRole("button", { name: "Recents" }));
    expect(within(recents).queryByText("No recent chats")).not.toBeInTheDocument();
    expect(within(recents).getByRole("button", { name: "Recents" })).toBeInTheDocument();
  });
  it.each(["chat", "work", "study", "code"] as const)("includes project sessions in %s Recents without mixing Home and Code", async mode => {
    useWorkspaceModeStore.setState({ mode });
    await setup([
      { id: "own", title: "Current project task", project_id: "p1", workspace_mode: mode },
      { id: "other-mode", title: "Other mode task", project_id: "p1", workspace_mode: mode === "code" ? "chat" : "code" },
      { id: "archived", title: "Archived", project_id: "p1", workspace_mode: mode, archived: true },
    ]);
    const recents = screen.getByRole("region", { name: "Recent chats" });
    expect(within(recents).getByText("Current project task")).toBeInTheDocument();
    expect(within(recents).queryByText("Other mode task")).not.toBeInTheDocument();
    expect(screen.queryByText("Archived")).not.toBeInTheDocument();
    expect(within(recents).getByText("Current project task").closest(".project-sidebar-row")).not.toHaveClass("project-session-row");
  });
  it("expands project sessions without filtering Recents, selecting a project or navigating away", async () => {
    const { container, open } = await setup();
    const projects = screen.getByRole("region", { name: "Projects" });
    const recents = screen.getByRole("region", { name: "Recent chats" });
    expect(within(projects).queryByText("Project task")).not.toBeInTheDocument();
    expect(within(recents).getByText("Project task")).toBeInTheDocument();
    expect(within(recents).getByText("Pinned task")).toBeInTheDocument();
    expect(within(recents).getByText("Loose task")).toBeInTheDocument();
    expect(container.querySelector('[data-sidebar-lists]')).toHaveClass('grid-rows-[minmax(0,1fr)_minmax(0,1fr)]');
    const projectScroll = container.querySelector('[data-project-list]')!;
    const recentScroll = container.querySelector('[data-recent-list]')!;
    expect(projectScroll).toHaveClass('overflow-y-auto');
    expect(recentScroll).toHaveClass('overflow-y-auto');
    projectScroll.scrollTop = 80;
    fireEvent.scroll(projectScroll);
    expect(recentScroll.scrollTop).toBe(0);
    fireEvent.click(within(recents).getByText('Project task'));
    expect(open).toHaveBeenCalledWith('b');
    fireEvent.click(screen.getByRole('button', { name: 'Browser' }));
    expect(useProjectStore.getState().pendingProjectId).toBe('p1');
    expect(screen.getByRole('button', { name: 'Browser' })).toHaveAttribute('aria-expanded', 'true');
    expect(within(projects).getByText('No chats in this project')).toBeInTheDocument();
    expect(within(recents).getByText('Project task')).toBeInTheDocument();
    expect(screen.getByTestId('location')).not.toHaveTextContent('"selectedProjectId":"p2"');
    fireEvent.click(screen.getByRole('button', { name: 'Desktop' }));
    expect(within(projects).getByText('Project task')).toBeInTheDocument();
    expect(within(projects).getByText('Chat mode task')).toBeInTheDocument();
    expect(within(projects).queryByText('Archived task')).not.toBeInTheDocument();
    fireEvent.click(within(projects).getByText('Project task'));
    expect(open).toHaveBeenCalledWith('b');
    fireEvent.click(screen.getByRole('button', { name: 'Desktop' }));
    expect(within(projects).queryByText('Project task')).not.toBeInTheDocument();
    expect(within(recents).getByText('Project task')).toBeInTheDocument();
    expect(within(projects).queryByRole('button', { name: 'No project' })).not.toBeInTheDocument();
    expect(useProjectStore.getState().pendingProjectId).toBe('p1');
    expect(within(recents).getByText('Loose task')).toBeInTheDocument();
  });
  it('ignores stale project refresh responses after a newer selection and refresh', async () => {
    let resolveOld!: (value: typeof project[]) => void;
    vi.mocked(getProjects).mockImplementationOnce(() => new Promise(resolve => { resolveOld = resolve; }));
    render(<MemoryRouter><SidebarProjects chats={chats} streamingIds={new Set()} onOpenChat={vi.fn()} /></MemoryRouter>);
    act(() => window.dispatchEvent(new Event('projectsChanged')));
    await screen.findByRole('button', { name: 'Browser' });
    fireEvent.click(screen.getByRole('button', { name: 'Browser' }));
    await act(async () => resolveOld([{ ...project, name: 'Stale name' }]));
    expect(screen.queryByRole('button', { name: 'Stale name' })).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Browser' })).toHaveAttribute('aria-expanded', 'true');
    expect(useProjectStore.getState().pendingProjectId).toBe('p1');
  });
  it("preserves incoming recent order and Show more including pinned sessions", async () => {
    await setup(Array.from({ length: 7 }, (_, index) => ({ id: String(index), title: `Task ${index}`, project_id: 'p1', workspace_mode: 'code', pinned: index === 3 })));
    const recents = screen.getByRole('region', { name: 'Recent chats' });
    expect(within(recents).queryByText('Task 5')).not.toBeInTheDocument();
    fireEvent.click(within(recents).getByText('Show more'));
    expect([...recents.querySelectorAll('[data-session-id]')].map(row => row.getAttribute('data-session-id'))).toEqual(['0', '1', '2', '3', '4', '5', '6']);
  });
  it("shows both empty states without hiding the list regions", async () => {
    vi.mocked(getProjects).mockResolvedValue([]);
    render(<MemoryRouter><SidebarProjects chats={[]} streamingIds={new Set()} onOpenChat={vi.fn()} /></MemoryRouter>);
    expect(await screen.findByText('No projects yet')).toBeInTheDocument();
    expect(screen.getByText('No recent chats')).toBeInTheDocument();
  });
  it("executes the pin shortcut for the selected menu chat and ignores it while editing", async () => {
    await setup();
    chatMenu();
    fireEvent.keyDown(window, { key: "p", ctrlKey: true, altKey: true });
    await waitFor(() => expect(updateConversation).toHaveBeenCalledWith("b", { pinned: true }));
    await waitFor(() => expect(screen.queryByRole("menu")).not.toBeInTheDocument());
    chatMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: /^Rename/ }));
    fireEvent.keyDown(screen.getByLabelText("Chat name"), { key: "p", ctrlKey: true, altKey: true });
    expect(updateConversation).toHaveBeenCalledTimes(1);
  });
  it("saves pin changes through the API and reports failure without hiding the chat", async () => {
    await setup();
    chatMenu();
    vi.mocked(updateConversation).mockRejectedValueOnce(
      new Error("Could not save pin"),
    );
    fireEvent.click(screen.getByRole("menuitem", { name: /^Pin / }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Could not save pin",
    );
    expect(updateConversation).toHaveBeenCalledWith("b", { pinned: true });
    expect(within(screen.getByRole("region", { name: "Recent chats" })).getByText("Project task")).toBeInTheDocument();
  });
  it("moves a chat and can detach it without deleting it", async () => {
    await setup();
    chatMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Project" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Browser" }));
    await waitFor(() =>
      expect(updateConversation).toHaveBeenCalledWith("b", {
        project_id: "p2",
      }),
    );
    await waitFor(() =>
      expect(screen.queryByRole("menu")).not.toBeInTheDocument(),
    );
    chatMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Project" }));
    fireEvent.click(
      screen.getByRole("menuitem", { name: "Remove from Desktop" }),
    );
    await waitFor(() =>
      expect(updateConversation).toHaveBeenCalledWith("b", {
        project_id: null,
      }),
    );
  });
  it("requires folder trust before moving to an untrusted project", async () => {
    vi.mocked(getProjects).mockResolvedValue([
      project,
      { ...other, folders: [{ path: "C:\\work\\browser", trusted: false }] },
    ]);
    await setup();
    chatMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Project" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Browser" }));
    expect(
      await screen.findByRole("dialog", { name: "Trust this folder?" }),
    ).toBeInTheDocument();
    expect(updateConversation).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(updateConversation).not.toHaveBeenCalled();
  });
  it("edits the name and source folders without saving draft changes on cancel", async () => {
    await setup();
    projectMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Edit" }));
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Renamed" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(updateProject).not.toHaveBeenCalled();
    projectMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Edit" }));
    fireEvent.change(screen.getByLabelText("Project name"), {
      target: { value: "Renamed" },
    });
    fireEvent.click(screen.getByRole("button", { name: "Add folder" }));
    // Browsers pick folders on the server; typing a path and opening it works too.
    const path = await screen.findByLabelText("Folder path");
    fireEvent.change(path, { target: { value: "C:\\work\\second" } });
    fireEvent.click(screen.getByRole("button", { name: "Open" }));
    const use = await screen.findByRole("button", { name: "Use this folder" });
    // Pointer and Escape handling of the popup must ignore the picker above it.
    fireEvent.pointerDown(use);
    fireEvent.click(use);
    fireEvent.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(updateProject).toHaveBeenCalledWith("p1", {
        name: "Renamed",
        folders: [project.workspace_path, "C:\\work\\second"],
      }),
    );
  });
  it("archives all project chats including pinned chats but never deletes messages", async () => {
    await setup();
    projectMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Archive chats" }));
    await waitFor(() => expect(updateConversation).toHaveBeenCalledTimes(3));
    expect(updateConversation).toHaveBeenCalledWith("a", { archived: true });
    expect(updateConversation).toHaveBeenCalledWith("b", { archived: true });
    expect(updateConversation).toHaveBeenCalledWith("e", { archived: true });
  });
  it("requires confirmation to remove a project and preserves cancellation", async () => {
    await setup();
    projectMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Remove project" }));
    expect(
      screen.getByText(/files on disk are not deleted/),
    ).toBeInTheDocument();
    expect(deleteProject).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "Cancel" }));
    expect(deleteProject).not.toHaveBeenCalled();
  });
  it("persists unread state and clears it when opening that chat", async () => {
    const { open } = await setup();
    chatMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: /^Mark as unread/ }));
    expect(useSidebarPreferences.getState().unreadChats).toContain("b");
    expect(
      JSON.parse(localStorage.getItem("project-sidebar-preferences")!).state
        .unreadChats,
    ).toContain("b");
    fireEvent.click(within(screen.getByRole("region", { name: "Recent chats" })).getByText("Project task"));
    expect(open).toHaveBeenCalledWith("b");
    expect(useSidebarPreferences.getState().unreadChats).not.toContain("b");
  });
  it("copies complete Markdown content, including structured message text", async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    Object.defineProperty(navigator, "clipboard", {
      value: { writeText },
      configurable: true,
    });
    vi.mocked(getConversation).mockResolvedValue({
      messages: [
        {
          role: "assistant",
          content: '[{"type":"text","text":"Full answer"}]',
        },
      ],
    } as never);
    await setup();
    chatMenu();
    fireEvent.click(screen.getByRole("menuitem", { name: "Copy" }));
    fireEvent.click(screen.getByRole("menuitem", { name: "Copy as Markdown" }));
    await waitFor(() =>
      expect(writeText).toHaveBeenCalledWith(
        "# Project task\n\n## assistant\n\nFull answer",
      ),
    );
  });
});
