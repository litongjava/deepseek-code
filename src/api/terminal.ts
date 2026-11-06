import { request } from './client';

export interface TerminalSession {
  id: string;
  shell: string;
  cwd: string;
  pid: number;
}

export async function createTerminal(cwd?: string, shell?: string): Promise<TerminalSession> {
  const res = await request('/terminal/create', {
    method: 'POST', body: JSON.stringify({ cwd, shell }),
  });
  return res.json();
}

const pendingWrites = new Map<string, Promise<void>>();

export function writeTerminal(terminalId: string, data: string): Promise<void> {
  // HTTP requests may arrive out of order; preserve the terminal's keystream.
  const pending = (pendingWrites.get(terminalId) ?? Promise.resolve()).then(async () => {
    await request('/terminal/write', {
      method: 'POST', body: JSON.stringify({ session_id: terminalId, data }),
    });
  });
  pendingWrites.set(terminalId, pending);
  const clear = () => { if (pendingWrites.get(terminalId) === pending) pendingWrites.delete(terminalId); };
  void pending.then(clear, clear);
  return pending;
}

export async function resizeTerminal(terminalId: string, cols: number, rows: number): Promise<void> {
  await request('/terminal/resize', {
    method: 'POST', body: JSON.stringify({ session_id: terminalId, cols, rows }),
  });
}

export async function closeTerminal(terminalId: string): Promise<void> {
  await request('/terminal/close', { method: 'POST', body: JSON.stringify(terminalId) });
}

export async function listTerminals(): Promise<TerminalSession[]> {
  const res = await request('/terminal/list');
  return (await res.json()).sessions ?? [];
}

export function streamTerminalOutput(
  terminalId: string,
  onData: (data: string) => void,
  onExit: (code: number | null) => void,
  onError: (err: string) => void,
  signal?: AbortSignal
): () => void {
  const controller = new AbortController();
  const abort = () => controller.abort();
  signal?.addEventListener('abort', abort, { once: true });
  if (signal?.aborted) abort();

  void (async () => {
    let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
    try {
      const response = await request(`/terminal/${encodeURIComponent(terminalId)}/stream`, {
        signal: controller.signal,
      });
      if (!response.body) throw new Error('Terminal stream has no response body');
      reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      let event = '';
      let data: string[] = [];
      const dispatch = () => {
        if (!data.length) { event = ''; return false; }
        const payload = JSON.parse(data.join('\n'));
        const type = payload.type || event;
        data = [];
        event = '';
        if (type === 'exit') {
          onExit(typeof payload.code === 'number' ? payload.code : null);
          return true;
        }
        if (type === 'error') throw new Error(payload.data || 'Terminal stream interrupted');
        if (typeof payload.data === 'string') onData(payload.data);
        return false;
      };
      while (!controller.signal.aborted) {
        const { done, value } = await reader.read();
        if (controller.signal.aborted) return;
        buffer += decoder.decode(value, { stream: !done });
        let newline: number;
        while ((newline = buffer.indexOf('\n')) >= 0) {
          const line = buffer.slice(0, newline).replace(/\r$/, '');
          buffer = buffer.slice(newline + 1);
          if (!line) {
            if (dispatch()) return;
          } else if (line.startsWith('event:')) {
            event = line.slice(6).trimStart();
          } else if (line.startsWith('data:')) {
            data.push(line.slice(5).replace(/^ /, ''));
          }
        }
        if (done) throw new Error('Terminal stream disconnected; process status is unknown');
      }
    } catch (error) {
      if (!controller.signal.aborted) onError(error instanceof Error ? error.message : String(error));
    } finally {
      await reader?.cancel().catch(() => {});
      reader?.releaseLock();
      signal?.removeEventListener('abort', abort);
    }
  })();
  return () => {
    signal?.removeEventListener('abort', abort);
    controller.abort();
  };
}
