import React, { forwardRef, useState, useEffect, useRef, useCallback, useImperativeHandle } from 'react';
import { Terminal, Plus, X, RefreshCw } from 'lucide-react';
import { Terminal as XTerm } from 'xterm';
import { FitAddon } from 'xterm-addon-fit';
import { createTerminal, writeTerminal, resizeTerminal, closeTerminal, streamTerminalOutput } from '../api/terminal';
import type { TerminalSession } from '../api/terminal';
import { isTauri } from '@tauri-apps/api/core';
import { confirm } from '@tauri-apps/plugin-dialog';
import 'xterm/css/xterm.css';

export interface TerminalPanelHandle {
  requestClose(): Promise<boolean>;
}

export interface TerminalPanelProps {
  onClose: () => void;
  cwd?: string;
  active?: boolean;
}

interface TerminalTab extends TerminalSession {
  exited: boolean;
}

function TerminalBody({ tab, active, onExit, onError }: {
  tab: TerminalTab;
  active: boolean;
  onExit: (id: string) => void;
  onError: (message: string) => void;
}) {
  const container = useRef<HTMLDivElement>(null);
  const terminal = useRef<{ xterm: XTerm; fit: FitAddon } | undefined>(undefined);
  const activeRef = useRef(active);
  activeRef.current = active;

  useEffect(() => {
    const element = container.current!;
    const xterm = new XTerm({
      cursorBlink: true,
      fontSize: 13,
      fontFamily: '"JetBrains Mono", "Fira Code", "Cascadia Code", Menlo, Monaco, monospace',
      theme: { background: '#1e1e2e', foreground: '#cdd6f4', cursor: '#f5e0dc', selectionBackground: '#585b7066' },
    });
    const fit = new FitAddon();
    xterm.loadAddon(fit);
    xterm.open(element);
    terminal.current = { xterm, fit };
    const input = xterm.onData(data => { void writeTerminal(tab.id, data).catch(error => onError(String(error))); });
    const resize = xterm.onResize(({ cols, rows }) => {
      void resizeTerminal(tab.id, cols, rows).catch(error => onError(String(error)));
    });
    const stop = streamTerminalOutput(tab.id, data => xterm.write(data), code => {
      onExit(tab.id);
      xterm.options.disableStdin = true;
      xterm.writeln(`\r\n[Process exited${code === null ? '; exit code unavailable' : ` with code ${code}`} ]`);
    }, onError);
    const fitVisible = () => {
      if (activeRef.current && element.clientWidth > 0 && element.clientHeight > 0) fit.fit();
    };
    const observer = new ResizeObserver(fitVisible);
    observer.observe(element);
    fitVisible();
    return () => {
      observer.disconnect();
      stop();
      input.dispose();
      resize.dispose();
      terminal.current = undefined;
      xterm.dispose();
    };
  }, [tab.id, onExit, onError]);

  useEffect(() => {
    if (active && terminal.current && container.current?.clientWidth) {
      terminal.current.fit.fit();
      terminal.current.xterm.focus();
    }
  }, [active]);

  return <div ref={container} className={`absolute inset-0 ${active ? '' : 'hidden'}`}
    aria-label={`Terminal ${tab.cwd}`} onClick={() => terminal.current?.xterm.focus()} />;
}

const TerminalPanel = forwardRef<TerminalPanelHandle, TerminalPanelProps>(({ onClose, cwd, active = true }, ref) => {
  const [tabs, setTabs] = useState<TerminalTab[]>([]);
  const [activeId, setActiveId] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [closing, setClosing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const tabsRef = useRef<TerminalTab[]>([]);
  const mounted = useRef(false);
  const creatingRef = useRef(false);
  const closingRef = useRef(false);
  const cwdRef = useRef(cwd);
  cwdRef.current = cwd;

  const reportError = useCallback((message: string) => {
    if (mounted.current) setError(message);
  }, []);
  const updateTabs = useCallback((next: TerminalTab[]) => {
    tabsRef.current = next;
    if (mounted.current) setTabs(next);
  }, []);
  const markExited = useCallback((id: string) => {
    updateTabs(tabsRef.current.map(tab => tab.id === id ? { ...tab, exited: true } : tab));
  }, [updateTabs]);

  const createNewTerminal = useCallback(async () => {
    if (creatingRef.current || closingRef.current) return;
    creatingRef.current = true;
    setCreating(true);
    setError(null);
    try {
      const session = await createTerminal(cwdRef.current);
      if (!mounted.current) {
        await closeTerminal(session.id);
        return;
      }
      updateTabs([...tabsRef.current, { ...session, exited: false }]);
      setActiveId(session.id);
    } catch (reason) {
      reportError(String(reason));
    } finally {
      creatingRef.current = false;
      if (mounted.current) setCreating(false);
    }
  }, [reportError, updateTabs]);

  useEffect(() => {
    mounted.current = true;
    let disposed = false;
    // Wait for the committed mount, not a guessed DOM timeout (also avoids StrictMode double-spawn).
    queueMicrotask(() => { if (!disposed) void createNewTerminal(); });
    return () => {
      disposed = true;
      mounted.current = false;
      const owned = tabsRef.current;
      tabsRef.current = [];
      for (const tab of owned) {
        void closeTerminal(tab.id).catch(reason => console.error('[Terminal] Unmount cleanup failed:', reason));
      }
    };
  }, [createNewTerminal]);

  const closeTabs = useCallback(async (ids: string[]): Promise<boolean> => {
    if (creatingRef.current || closingRef.current) return false;
    const targets = tabsRef.current.filter(tab => ids.includes(tab.id));
    closingRef.current = true;
    setClosing(true);
    setError(null);
    try {
      if (targets.some(tab => !tab.exited)) {
        const message = '关闭终端会结束其中正在运行的 shell 和任务。确定关闭？';
        const approved = await (isTauri() ? confirm(message, { title: 'Close terminal', kind: 'warning' }) : window.confirm(message));
        if (!approved) return false;
      }
      for (const tab of targets) {
        await closeTerminal(tab.id);
        const remaining = tabsRef.current.filter(item => item.id !== tab.id);
        updateTabs(remaining);
        setActiveId(current => current === tab.id ? remaining[remaining.length - 1]?.id ?? null : current);
      }
      return true;
    } catch (reason) {
      reportError(`终端关闭失败：${String(reason)}`);
      return false;
    } finally {
      closingRef.current = false;
      if (mounted.current) setClosing(false);
    }
  }, [reportError, updateTabs]);

  const requestClose = useCallback(() => closeTabs(tabsRef.current.map(tab => tab.id)), [closeTabs]);
  useImperativeHandle(ref, () => ({ requestClose }), [requestClose]);

  return (
    <div className="flex flex-col h-full min-h-0 bg-[#1e1e2e]">
      <div className="flex items-center bg-[#181825] border-b border-[#313244] px-2 min-h-[36px] shrink-0">
        <div className="flex items-center gap-1 flex-1 overflow-x-auto">
          {tabs.map((tab, index) => (
            <div key={tab.id} className={`flex items-center shrink-0 rounded-md ${tab.id === activeId ? 'bg-[#313244]' : ''}`}>
              <button onClick={() => setActiveId(tab.id)} title={`${tab.shell} · ${tab.cwd}`}
                className="flex items-center gap-1.5 px-2 py-1 text-xs text-[#cdd6f4]">
                <Terminal size={12} />Terminal {index + 1}{tab.exited ? ' (exited)' : ''}
              </button>
              <button aria-label={`关闭终端 ${index + 1}`} disabled={closing || creating}
                onClick={() => void closeTabs([tab.id])} className="p-1 text-[#a6adc8]"><X size={12} /></button>
            </div>
          ))}
          <button onClick={() => void createNewTerminal()} disabled={creating || closing}
            aria-label="新建终端" title="新建终端" className="p-1 text-[#a6adc8] disabled:opacity-50 shrink-0">
            {creating ? <RefreshCw size={14} className="animate-spin" /> : <Plus size={14} />}
          </button>
        </div>
        <button onClick={async () => { if (await requestClose()) onClose(); }} disabled={creating || closing}
          aria-label="关闭终端面板" title="关闭终端面板" className="p-1.5 text-[#a6adc8] shrink-0 ml-2"><X size={14} /></button>
      </div>
      {error && <div role="alert" className="text-xs text-red-300 px-2 py-1 break-words">{error}</div>}
      {tabs.length === 0 && !creating && <div className="text-xs text-[#a6adc8] p-4">点击 + 新建终端</div>}
      <div className="flex-1 min-h-0 overflow-hidden relative">
        {tabs.map(tab => <TerminalBody key={tab.id} tab={tab} active={active && tab.id === activeId}
          onExit={markExited} onError={reportError} />)}
      </div>
    </div>
  );
});

TerminalPanel.displayName = 'TerminalPanel';
export default TerminalPanel;
