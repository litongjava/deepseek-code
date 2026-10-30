import { act, fireEvent, render, screen } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { useUIStore } from '../../stores/useUIStore';
import { RIGHT_SIDEBAR_DEFAULT_WIDTH, useRightSidebarStore } from '../../stores/useRightSidebarStore';
import RightSidebarBrowserTab from './RightSidebarBrowserTab';
import { LOAD_HINT_DELAY_MS } from './browser/EmbeddedBrowser';

const tauri = vi.hoisted(() => ({ isTauri: false, openExternal: vi.fn(async (_url: string) => {}) }));
vi.mock('../../utils/tauriAPI', () => ({ tauriAPI: tauri }));

const frame = () => screen.getByTitle(/^Web page: /) as HTMLIFrameElement;
const address = () => screen.getByRole('textbox', { name: 'Address' }) as HTMLInputElement;
const persisted = () => JSON.parse(localStorage.getItem('right-sidebar') || '{}').state;
const hint = () => screen.queryByText(/site may block embedding/);

function submitAddress(value: string) {
  fireEvent.change(address(), { target: { value } });
  fireEvent.submit(address().closest('form')!);
}

beforeEach(() => {
  localStorage.clear();
  tauri.isTauri = false;
  tauri.openExternal.mockClear();
  useUIStore.setState({ language: 'en' });
  useRightSidebarStore.setState({ open: true, tab: 'browser', width: RIGHT_SIDEBAR_DEFAULT_WIDTH, browserUrl: '', browserRequest: 0 });
});

afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe('right sidebar browser tab', () => {
  it('starts with an address field and local dev suggestions', () => {
    render(<RightSidebarBrowserTab />);
    expect(screen.getByText('No page open')).toBeInTheDocument();
    expect(screen.queryByTitle(/^Web page: /)).toBeNull();
    for (const port of ['5173', '3000', '8080']) {
      expect(screen.getByRole('button', { name: `localhost:${port}` })).toBeInTheDocument();
    }
    fireEvent.click(screen.getByRole('button', { name: 'localhost:3000' }));
    expect(frame()).toHaveAttribute('src', 'http://localhost:3000');
    expect(persisted().browserUrl).toBe('http://localhost:3000');
  });

  it('normalizes typed input and rejects unsafe schemes inline', () => {
    render(<RightSidebarBrowserTab />);
    submitAddress('javascript:alert(1)');
    expect(screen.getByRole('alert')).toHaveTextContent('Only http and https pages can be opened here.');
    expect(address()).toHaveAttribute('aria-invalid', 'true');
    expect(screen.queryByTitle(/^Web page: /)).toBeNull();

    submitAddress('localhost:5173');
    expect(frame()).toHaveAttribute('src', 'http://localhost:5173/');
    expect(address().value).toBe('http://localhost:5173/');
  });

  it('navigates from the address bar and walks its history with Back, Forward and Reload', () => {
    useRightSidebarStore.setState({ browserUrl: 'http://localhost:5173/' });
    render(<RightSidebarBrowserTab />);
    expect(frame()).toHaveAttribute('src', 'http://localhost:5173/');
    expect(frame()).toHaveAttribute('sandbox', 'allow-scripts allow-same-origin allow-forms allow-popups allow-popups-to-escape-sandbox allow-modals allow-downloads');
    expect(frame()).toHaveAttribute('referrerpolicy', 'no-referrer');
    const back = screen.getByRole('button', { name: 'Back' });
    const forward = screen.getByRole('button', { name: 'Forward' });
    expect(back).toBeDisabled();
    expect(forward).toBeDisabled();

    submitAddress('example.com/docs');
    expect(frame()).toHaveAttribute('src', 'https://example.com/docs');
    expect(persisted().browserUrl).toBe('https://example.com/docs');

    fireEvent.click(back);
    expect(frame()).toHaveAttribute('src', 'http://localhost:5173/');
    expect(address().value).toBe('http://localhost:5173/');
    expect(persisted().browserUrl).toBe('http://localhost:5173/');
    expect(back).toBeDisabled();

    fireEvent.click(forward);
    expect(frame()).toHaveAttribute('src', 'https://example.com/docs');

    const before = frame();
    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));
    expect(frame()).not.toBe(before);
    expect(frame()).toHaveAttribute('src', 'https://example.com/docs');

    fireEvent.click(screen.getByRole('button', { name: 'Zoom in' }));
    expect(screen.getByRole('button', { name: 'Reset zoom (125%)' })).toBeInTheDocument();

    fireEvent.click(screen.getByRole('button', { name: 'Close page' }));
    expect(screen.getByText('No page open')).toBeInTheDocument();
    expect(persisted().browserUrl).toBe('');
  });

  it('follows navigation requests from the store, even to the same URL', () => {
    useRightSidebarStore.setState({ browserUrl: 'http://localhost:5173/' });
    render(<RightSidebarBrowserTab />);
    act(() => useRightSidebarStore.getState().openBrowser('https://example.com/'));
    expect(frame()).toHaveAttribute('src', 'https://example.com/');
    const before = frame();
    act(() => useRightSidebarStore.getState().openBrowser('https://example.com/'));
    expect(frame()).not.toBe(before);
    fireEvent.click(screen.getByRole('button', { name: 'Back' }));
    expect(frame()).toHaveAttribute('src', 'https://example.com/');
    fireEvent.click(screen.getByRole('button', { name: 'Back' }));
    expect(frame()).toHaveAttribute('src', 'http://localhost:5173/');
  });

  it('opens the page in the system browser through Tauri or a new tab', async () => {
    useRightSidebarStore.setState({ browserUrl: 'http://localhost:5173/' });
    const open = vi.spyOn(window, 'open').mockReturnValue(null);
    render(<RightSidebarBrowserTab />);
    fireEvent.click(screen.getByRole('button', { name: 'Open in system browser' }));
    expect(open).toHaveBeenCalledWith('http://localhost:5173/', '_blank', 'noopener,noreferrer');
    expect(tauri.openExternal).not.toHaveBeenCalled();

    tauri.isTauri = true;
    await act(async () => { fireEvent.click(screen.getByRole('button', { name: 'Open in system browser' })); });
    expect(tauri.openExternal).toHaveBeenCalledWith('http://localhost:5173/');
    expect(open).toHaveBeenCalledTimes(1);
  });

  it('hints that a local page may block embedding only when it never loads', () => {
    vi.useFakeTimers();
    useRightSidebarStore.setState({ browserUrl: 'http://localhost:5173/' });
    render(<RightSidebarBrowserTab />);
    fireEvent.load(frame());
    act(() => { vi.advanceTimersByTime(LOAD_HINT_DELAY_MS); });
    expect(hint()).toBeNull();

    fireEvent.click(screen.getByRole('button', { name: 'Reload' }));
    act(() => { vi.advanceTimersByTime(LOAD_HINT_DELAY_MS - 1); });
    expect(hint()).toBeNull();
    act(() => { vi.advanceTimersByTime(1); });
    expect(hint()).toBeInTheDocument();
    expect(screen.getByRole('status')).toContainElement(screen.getByRole('button', { name: 'Open in browser' }));
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss' }));
    expect(hint()).toBeNull();
  });

  it('always hints for remote hosts once the frame has loaded', () => {
    useRightSidebarStore.setState({ browserUrl: 'https://example.com/' });
    render(<RightSidebarBrowserTab />);
    expect(hint()).toBeNull();
    fireEvent.load(frame());
    expect(hint()).toBeInTheDocument();
  });

  it('restores the last page after a reload', async () => {
    const { unmount } = render(<RightSidebarBrowserTab />);
    submitAddress('localhost:8080');
    unmount();
    const saved = localStorage.getItem('right-sidebar')!;
    useRightSidebarStore.setState({ browserUrl: '' });
    localStorage.setItem('right-sidebar', saved);
    await act(async () => { await useRightSidebarStore.persist.rehydrate(); });
    render(<RightSidebarBrowserTab />);
    expect(frame()).toHaveAttribute('src', 'http://localhost:8080/');
  });

  it('ignores invalid persisted URLs', async () => {
    localStorage.setItem('right-sidebar', JSON.stringify({ version: 1, state: { open: true, tab: 'browser', width: 360, browserUrl: 'javascript:alert(1)' } }));
    await act(async () => { await useRightSidebarStore.persist.rehydrate(); });
    expect(useRightSidebarStore.getState().browserUrl).toBe('');
  });
});
