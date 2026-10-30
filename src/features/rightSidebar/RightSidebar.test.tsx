import React, { useRef, useState } from 'react';
import { act, fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { DocumentInfo } from '../../components/DocumentCard';
import { useUIStore } from '../../stores/useUIStore';
import {
  RIGHT_SIDEBAR_DEFAULT_WIDTH, clampRightSidebarWidth, isRightSidebarRoute, useRightSidebarStore,
} from '../../stores/useRightSidebarStore';
import RightPanelSlot from './RightPanelSlot';
import RightSidebarToggle from './RightSidebarToggle';
import { useRightSidebarController } from './useRightSidebarController';

vi.mock('../../components/CodePanel', () => ({
  default: ({ document, onClose }: { document: DocumentInfo; onClose: () => void }) => (
    <div>Preview {document.title}<button type="button" onClick={onClose}>Close preview</button></div>
  ),
}));
vi.mock('../../components/ArtifactsPanel', () => ({
  default: ({ onClose }: { onClose: () => void }) => <div>Artifacts list<button type="button" onClick={onClose}>Close artifacts</button></div>,
}));

const doc = { id: 'doc-1', title: 'Report', filename: 'report.md', url: '/report.md' } as unknown as DocumentInfo;

// Mirrors the composition in App.tsx: title-bar toggle, controller and the shared right-panel slot.
function Harness({ pathname = '/' }: { pathname?: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const [documentPanelDoc, setDocumentPanelDoc] = useState<DocumentInfo | null>(null);
  const [showArtifacts, setShowArtifacts] = useState(false);
  const allowed = isRightSidebarRoute(pathname);
  const sidebar = useRightSidebarController({
    allowed,
    otherPanelOpen: !!documentPanelDoc || showArtifacts,
    closeOtherPanels: () => { setDocumentPanelDoc(null); setShowArtifacts(false); },
  });
  return (
    <MemoryRouter>
      <div ref={containerRef}>
        {allowed && <RightSidebarToggle expanded={sidebar.visible} onToggle={sidebar.toggle} />}
        <button type="button" onClick={() => setDocumentPanelDoc(doc)}>Open preview</button>
        <button type="button" onClick={() => setShowArtifacts(true)}>Open artifacts</button>
        <RightPanelSlot
          containerRef={containerRef}
          documentPanelDoc={documentPanelDoc}
          documentPanelWidth={50}
          onDocumentPanelResize={() => {}}
          onCloseDocument={() => setDocumentPanelDoc(null)}
          showArtifacts={showArtifacts}
          artifacts={[]}
          onCloseArtifacts={() => setShowArtifacts(false)}
          onOpenDocument={setDocumentPanelDoc}
          sidebarVisible={sidebar.visible}
          onCloseSidebar={sidebar.close}
        />
      </div>
    </MemoryRouter>
  );
}

const persisted = () => JSON.parse(localStorage.getItem('right-sidebar') || '{}').state;
const toggleButton = () => screen.getByRole('button', { name: 'Toggle right sidebar' });
const sidebar = () => screen.queryByRole('complementary', { name: 'Right sidebar' });

beforeEach(() => {
  localStorage.clear();
  useUIStore.setState({ language: 'en' });
  useRightSidebarStore.setState({ open: false, tab: 'files', width: RIGHT_SIDEBAR_DEFAULT_WIDTH });
});

describe('right sidebar', () => {
  it('opens and closes from the toggle and persists across reloads', async () => {
    const { unmount } = render(<Harness />);
    expect(sidebar()).toBeNull();
    expect(toggleButton()).toHaveAttribute('aria-expanded', 'false');

    fireEvent.click(toggleButton());
    expect(sidebar()).toBeInTheDocument();
    expect(toggleButton()).toHaveAttribute('aria-expanded', 'true');
    expect(toggleButton()).toHaveAttribute('aria-controls', 'right-sidebar');
    expect(persisted()).toMatchObject({ open: true, tab: 'files', width: RIGHT_SIDEBAR_DEFAULT_WIDTH });

    unmount();
    useRightSidebarStore.setState({ open: false }, false);
    localStorage.setItem('right-sidebar', JSON.stringify({ version: 1, state: { open: true, tab: 'browser', width: 420 } }));
    await act(async () => { await useRightSidebarStore.persist.rehydrate(); });
    render(<Harness />);
    expect(sidebar()).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Browser' })).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByTestId('right-panel-slot')).toHaveStyle({ width: '420px' });

    fireEvent.click(screen.getByRole('button', { name: 'Close right sidebar' }));
    expect(sidebar()).toBeNull();
    expect(persisted().open).toBe(false);

    fireEvent.keyDown(window, { key: 'b', ctrlKey: true, altKey: true });
    expect(sidebar()).toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'B', ctrlKey: true, altKey: true, shiftKey: true });
    expect(sidebar()).toBeInTheDocument();
    fireEvent.keyDown(window, { key: 'b', metaKey: true, altKey: true });
    expect(sidebar()).toBeNull();
  });

  it('switches tabs by click and arrow keys and remembers the tab', () => {
    useRightSidebarStore.setState({ open: true });
    render(<Harness />);
    const files = screen.getByRole('tab', { name: 'Files' });
    const browser = screen.getByRole('tab', { name: 'Browser' });
    expect(files).toHaveAttribute('aria-selected', 'true');
    expect(files).toHaveAttribute('tabindex', '0');
    expect(browser).toHaveAttribute('tabindex', '-1');
    expect(screen.getByRole('tabpanel', { name: 'Files' })).toHaveTextContent('This chat has no workspace folder');

    fireEvent.click(browser);
    expect(browser).toHaveAttribute('aria-selected', 'true');
    expect(screen.getByRole('tabpanel', { name: 'Browser' })).toHaveTextContent('No page open');
    expect(persisted().tab).toBe('browser');

    fireEvent.keyDown(browser, { key: 'ArrowRight' });
    expect(files).toHaveAttribute('aria-selected', 'true');
    expect(files).toHaveFocus();
    fireEvent.keyDown(files, { key: 'End' });
    expect(browser).toHaveFocus();
    fireEvent.keyDown(browser, { key: 'Home' });
    expect(screen.getByRole('tabpanel', { name: 'Files' })).toBeInTheDocument();
  });

  it('is hidden on full-page routes and ignores the shortcut there', () => {
    for (const route of ['/', '/chat/abc']) expect(isRightSidebarRoute(route)).toBe(true);
    for (const route of ['/settings', '/projects', '/customize', '/code', '/design', '/chats', '/artifacts', '/memory']) {
      expect(isRightSidebarRoute(route)).toBe(false);
    }
    useRightSidebarStore.setState({ open: true });
    render(<Harness pathname="/settings" />);
    expect(screen.queryByRole('button', { name: 'Toggle right sidebar' })).toBeNull();
    expect(sidebar()).toBeNull();
    expect(screen.getByTestId('right-panel-slot')).toHaveStyle({ width: '0px' });
    fireEvent.keyDown(window, { key: 'b', ctrlKey: true, altKey: true });
    expect(useRightSidebarStore.getState().open).toBe(true);
  });

  it('yields the slot to a document preview or artifacts and returns when they close', () => {
    useRightSidebarStore.setState({ open: true });
    render(<Harness />);
    expect(sidebar()).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Open preview' }));
    expect(screen.getByText('Preview Report')).toBeInTheDocument();
    expect(sidebar()).toBeNull();
    expect(toggleButton()).toHaveAttribute('aria-expanded', 'false');
    expect(screen.getByTestId('right-panel-slot')).toHaveStyle({ width: '50%' });

    fireEvent.click(screen.getByRole('button', { name: 'Close preview' }));
    expect(sidebar()).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Open artifacts' }));
    expect(screen.getByText('Artifacts list')).toBeInTheDocument();
    expect(sidebar()).toBeNull();
    fireEvent.click(screen.getByRole('button', { name: 'Close artifacts' }));
    expect(sidebar()).toBeInTheDocument();

    // Showing the sidebar explicitly dismisses the other panel instead of stacking.
    fireEvent.click(screen.getByRole('button', { name: 'Open preview' }));
    fireEvent.click(toggleButton());
    expect(screen.queryByText('Preview Report')).toBeNull();
    expect(sidebar()).toBeInTheDocument();
    expect(useRightSidebarStore.getState().open).toBe(true);
  });

  it('exposes English accessible names for every control', () => {
    useRightSidebarStore.setState({ open: true });
    render(<Harness />);
    expect(toggleButton()).toHaveAttribute('aria-keyshortcuts', 'Control+Alt+B');
    expect(screen.getByRole('tablist', { name: 'Right sidebar tabs' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Files' })).toBeInTheDocument();
    expect(screen.getByRole('tab', { name: 'Browser' })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Close right sidebar' })).toBeInTheDocument();
    expect(screen.getByRole('separator', { name: 'Resize right sidebar' })).toBeInTheDocument();
  });

  it('clamps width to 280px..min(900px, 70% of the viewport), including stored values', async () => {
    expect(clampRightSidebarWidth(100, 1600)).toBe(280);
    expect(clampRightSidebarWidth(2000, 1600)).toBe(900);
    expect(clampRightSidebarWidth(800, 1000)).toBe(700);
    expect(clampRightSidebarWidth(500, 300)).toBe(280);
    expect(clampRightSidebarWidth(Number.NaN, 1600)).toBe(RIGHT_SIDEBAR_DEFAULT_WIDTH);
    localStorage.setItem('right-sidebar', JSON.stringify({ version: 1, state: { open: 'yes', tab: 'terminal', width: 10 } }));
    await act(async () => { await useRightSidebarStore.persist.rehydrate(); });
    expect(useRightSidebarStore.getState()).toMatchObject({ open: false, tab: 'files', width: 280 });
  });
});
