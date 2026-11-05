import { fireEvent, render, screen } from '@testing-library/react';
import { MemoryRouter, useLocation } from 'react-router-dom';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import SidebarNavigation from './SidebarNavigation';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';

function Location() {
  return <output data-testid="location">{useLocation().pathname}</output>;
}

beforeEach(() => {
  const toolbar = document.createElement('div');
  toolbar.id = 'desktop-tool-menus';
  document.body.append(toolbar);
});
afterEach(() => document.getElementById('desktop-tool-menus')?.remove());

describe('extension navigation', () => {
  it.each([false, true])('opens customization without directory or standalone MCP (collapsed=%s)', collapsed => {
    useUIStore.getState().setLanguage('en');
    render(<MemoryRouter>
      <SidebarNavigation collapsed={collapsed} onNewChat={vi.fn()} onSearch={vi.fn()} onPanel={vi.fn()} onNavigate={vi.fn()} onSwarm={vi.fn()} />
      <Location />
    </MemoryRouter>);
    fireEvent.click(screen.getByRole('button', { name: en.sidebar.extensions }));
    expect(screen.getByRole('button', { name: en.sidebar.extensions }).closest('#desktop-tool-menus')).not.toBeNull();
    expect(screen.queryByRole('menuitem', { name: en.sidebar.directory })).not.toBeInTheDocument();
    expect(screen.queryByRole('menuitem', { name: 'MCP' })).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole('menuitem', { name: en.sidebar.customize }));
    expect(screen.getByTestId('location')).toHaveTextContent('/customize');
    if (collapsed) expect(screen.getByRole('button', { name: en.sidebar.extensions })).toHaveAttribute('aria-expanded', 'false');
  });
  it('keeps tools available with a collapsed sidebar and supports keyboard menu navigation', () => {
    useUIStore.getState().setLanguage('en');
    const panel = vi.fn();
    render(<MemoryRouter><SidebarNavigation collapsed onNewChat={vi.fn()} onSearch={vi.fn()} onPanel={panel} onNavigate={vi.fn()} onSwarm={vi.fn()} /></MemoryRouter>);
    const trigger = screen.getByRole('button', { name: 'Tools' });
    fireEvent.click(trigger);
    const terminal = screen.getByRole('menuitem', { name: en.sidebar.terminal });
    expect(terminal).toHaveFocus();
    fireEvent.keyDown(terminal, { key: 'End' });
    expect(screen.getByRole('menuitem', { name: en.sidebar.browser })).toHaveFocus();
    fireEvent.keyDown(window, { key: 'Escape' });
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
    expect(trigger).toHaveFocus();
    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole('menuitem', { name: en.sidebar.terminal }));
    expect(panel).toHaveBeenCalledWith('terminal');
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
    fireEvent.click(trigger);
    fireEvent.focusIn(screen.getByRole('button', { name: en.sidebar.newChat }));
    expect(screen.queryByRole('menu')).not.toBeInTheDocument();
  });
});
