import React, { createRef, StrictMode } from 'react';
import { act, cleanup, fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import TerminalPanel, { TerminalPanelHandle } from './TerminalPanel';
import * as api from '../api/terminal';

const terminalMocks = vi.hoisted(() => ({ dispose: vi.fn(), open: vi.fn(), fit: vi.fn(), streamStop: vi.fn() }));
vi.mock('xterm', () => ({ Terminal: class {
  options = {};
  loadAddon() {}
  open = terminalMocks.open;
  dispose = terminalMocks.dispose;
  focus() {}
  write() {}
  writeln() {}
  onData() { return { dispose: vi.fn() }; }
  onResize() { return { dispose: vi.fn() }; }
} }));
vi.mock('xterm-addon-fit', () => ({ FitAddon: class { fit = terminalMocks.fit; } }));
vi.mock('../api/terminal', () => ({
  createTerminal: vi.fn(), writeTerminal: vi.fn().mockResolvedValue(undefined),
  resizeTerminal: vi.fn().mockResolvedValue(undefined), closeTerminal: vi.fn(),
  streamTerminalOutput: vi.fn(),
}));

beforeEach(() => {
  vi.clearAllMocks();
  vi.stubGlobal('ResizeObserver', class { observe() {} disconnect() {} });
  vi.mocked(api.createTerminal).mockResolvedValue({ id: 't1', pid: 123, shell: 'fixture-shell', cwd: '/first' });
  vi.mocked(api.closeTerminal).mockResolvedValue(undefined);
  vi.mocked(api.streamTerminalOutput).mockReturnValue(terminalMocks.streamStop);
  vi.spyOn(window, 'confirm').mockReturnValue(true);
});
afterEach(() => { cleanup(); vi.restoreAllMocks(); });

async function ready() { await waitFor(() => expect(api.streamTerminalOutput).toHaveBeenCalled()); }

describe('TerminalPanel lifecycle', () => {
  it('creates only once under StrictMode and preserves shell across hidden/cwd changes', async () => {
    const onClose = vi.fn();
    const view = render(<StrictMode><TerminalPanel cwd="/first" onClose={onClose} /></StrictMode>);
    await ready();
    expect(api.createTerminal).toHaveBeenCalledTimes(1);
    expect(api.createTerminal).toHaveBeenCalledWith('/first');
    view.rerender(<StrictMode><TerminalPanel cwd="/second" active={false} onClose={onClose} /></StrictMode>);
    expect(api.closeTerminal).not.toHaveBeenCalled();
    expect(api.createTerminal).toHaveBeenCalledTimes(1);
    expect(screen.getByTitle('fixture-shell · /first')).toBeTruthy();
  });

  it('cancelled outer close keeps the session and stream intact', async () => {
    const ref = createRef<TerminalPanelHandle>();
    render(<TerminalPanel ref={ref} onClose={vi.fn()} />);
    await ready();
    vi.mocked(window.confirm).mockReturnValue(false);
    let result: boolean | undefined;
    await act(async () => { result = await ref.current!.requestClose(); });
    expect(result).toBe(false);
    expect(api.closeTerminal).not.toHaveBeenCalled();
    expect(terminalMocks.streamStop).not.toHaveBeenCalled();
  });

  it('failed close retains the tab and reports failure, then allows retry', async () => {
    const ref = createRef<TerminalPanelHandle>();
    const onClose = vi.fn();
    render(<TerminalPanel ref={ref} onClose={onClose} />);
    await ready();
    vi.mocked(api.closeTerminal).mockRejectedValueOnce(new Error('fixture failure'));
    let result: boolean | undefined;
    await act(async () => { result = await ref.current!.requestClose(); });
    expect(result).toBe(false);
    expect(screen.getByRole('alert').textContent).toContain('fixture failure');
    expect(screen.getByText('Terminal 1')).toBeTruthy();
    expect(terminalMocks.streamStop).not.toHaveBeenCalled();
    await act(async () => { result = await ref.current!.requestClose(); });
    expect(result).toBe(true);
    expect(terminalMocks.streamStop).toHaveBeenCalledOnce();
    expect(onClose).not.toHaveBeenCalled();
  });

  it('panel close confirms and calls onClose only after backend cleanup', async () => {
    const onClose = vi.fn();
    render(<TerminalPanel onClose={onClose} />);
    await ready();
    fireEvent.click(screen.getByRole('button', { name: '关闭终端面板' }));
    await waitFor(() => expect(onClose).toHaveBeenCalledOnce());
    expect(window.confirm).toHaveBeenCalledOnce();
    expect(api.closeTerminal).toHaveBeenCalledWith('t1');
  });

  it('confirmed process exit avoids an unnecessary live-shell confirmation', async () => {
    const ref = createRef<TerminalPanelHandle>();
    render(<TerminalPanel ref={ref} onClose={vi.fn()} />);
    await ready();
    act(() => { vi.mocked(api.streamTerminalOutput).mock.calls[0][2](7); });
    expect(screen.getByText('Terminal 1 (exited)')).toBeTruthy();
    await act(async () => { expect(await ref.current!.requestClose()).toBe(true); });
    expect(window.confirm).not.toHaveBeenCalled();
  });

  it('stream errors keep unknown process status active for close confirmation', async () => {
    const ref = createRef<TerminalPanelHandle>();
    render(<TerminalPanel ref={ref} onClose={vi.fn()} />);
    await ready();
    act(() => { vi.mocked(api.streamTerminalOutput).mock.calls[0][3]('disconnected'); });
    expect(screen.getByRole('alert').textContent).toContain('disconnected');
    vi.mocked(window.confirm).mockReturnValue(false);
    await act(async () => { expect(await ref.current!.requestClose()).toBe(false); });
    expect(api.closeTerminal).not.toHaveBeenCalled();
  });

  it('explicit unmount closes owned sessions and disposes stream/xterm', async () => {
    const view = render(<TerminalPanel onClose={vi.fn()} />);
    await ready();
    view.unmount();
    expect(api.closeTerminal).toHaveBeenCalledWith('t1');
    expect(terminalMocks.streamStop).toHaveBeenCalledOnce();
    expect(terminalMocks.dispose).toHaveBeenCalledOnce();
  });

  it('cleans a shell whose creation resolves after unmount', async () => {
    let finish!: (session: api.TerminalSession) => void;
    vi.mocked(api.createTerminal).mockReturnValueOnce(new Promise(resolve => { finish = resolve; }));
    const view = render(<TerminalPanel onClose={vi.fn()} />);
    await waitFor(() => expect(api.createTerminal).toHaveBeenCalledOnce());
    view.unmount();
    await act(async () => { finish({ id: 'late', pid: 1, shell: 'fixture', cwd: '/first' }); });
    expect(api.closeTerminal).toHaveBeenCalledWith('late');
    expect(api.streamTerminalOutput).not.toHaveBeenCalled();
  });
});
