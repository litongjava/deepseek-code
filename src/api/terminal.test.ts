import { beforeEach, describe, expect, it, vi } from 'vitest';
import { createTerminal, writeTerminal, resizeTerminal, closeTerminal, listTerminals, streamTerminalOutput } from './terminal';

vi.mock('../services/bridgeConnection', () => ({
  API_BASE: 'http://localhost/api', isTauriApp: false, hasBridge: false, hasNativeDesktop: () => false, bridgeOrigin: () => 'http://localhost', useDevProxy: false,
  waitForApiReady: vi.fn().mockResolvedValue(undefined), detectBridgePort: vi.fn(),
}));

const fetchMock = vi.fn();
beforeEach(() => {
  vi.stubGlobal('fetch', fetchMock);
  fetchMock.mockReset();
  localStorage.setItem('bridge_api_key', 'terminal-test-token');
});

function response(chunks: string[]) {
  const encoder = new TextEncoder();
  return new Response(new ReadableStream({
    start(controller) {
      chunks.forEach(chunk => controller.enqueue(encoder.encode(chunk)));
      controller.close();
    },
  }));
}

function receive() {
  const data = vi.fn();
  const exit = vi.fn();
  const error = vi.fn();
  const stop = streamTerminalOutput('a/b', data, exit, error);
  return { data, exit, error, stop };
}

describe('terminal API', () => {
  it('orders input per terminal while other terminals remain independent', async () => {
    let finish!: (value: Response) => void;
    fetchMock.mockImplementationOnce(() => new Promise<Response>(resolve => { finish = resolve; }));
    fetchMock.mockImplementation(() => Promise.resolve(Response.json({})));
    const first = writeTerminal('ordered', 'a');
    const second = writeTerminal('ordered', 'b');
    const enter = writeTerminal('ordered', '\r');
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());
    await writeTerminal('other', 'x');
    expect(fetchMock.mock.calls.map(([, options]) => JSON.parse(options.body).data)).toEqual(['a', 'x']);
    finish(Response.json({}));
    await Promise.all([first, second, enter]);
    expect(fetchMock.mock.calls.map(([, options]) => JSON.parse(options.body).data)).toEqual(['a', 'x', 'b', '\r']);
  });

  it('rejects queued input after a failed prefix and permits a fresh write', async () => {
    fetchMock.mockResolvedValueOnce(Response.json({ error: 'Write failed' }, { status: 500 }));
    const results = await Promise.allSettled([writeTerminal('failed', 'a'), writeTerminal('failed', 'b')]);
    expect(results.map(result => result.status)).toEqual(['rejected', 'rejected']);
    expect(fetchMock).toHaveBeenCalledOnce();
    fetchMock.mockResolvedValueOnce(Response.json({}));
    await writeTerminal('failed', '\u0003');
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it('authenticates every request with the shared client', async () => {
    fetchMock.mockImplementation(() => Promise.resolve(Response.json({ id: 't', sessions: [] })));
    await createTerminal('/fixture');
    await writeTerminal('t', 'pwd\r');
    await resizeTerminal('t', 90, 30);
    await closeTerminal('t');
    await listTerminals();
    expect(fetchMock).toHaveBeenCalledTimes(5);
    for (const [, options] of fetchMock.mock.calls) {
      expect(options.headers.Authorization).toBe('Bearer terminal-test-token');
    }
    expect(JSON.parse(fetchMock.mock.calls[0][1].body)).toEqual({ cwd: '/fixture' });
  });

  it('preserves error statuses instead of accepting failed mutations', async () => {
    fetchMock.mockResolvedValue(Response.json({ error: 'Terminal session not found' }, { status: 404 }));
    await expect(closeTerminal('missing')).rejects.toThrow('Terminal session not found');
  });

  it('parses split SSE frames and reports the actual nonzero exit code', async () => {
    fetchMock.mockResolvedValue(response([
      'event: data\r\ndata: {"type":"data","data":"中文"}\r\n\r\nevent: ex',
      'it\ndata: {"type":"exit",', '"code":7}\n\n',
    ]));
    const callbacks = receive();
    await vi.waitFor(() => expect(callbacks.exit).toHaveBeenCalledWith(7));
    expect(callbacks.data).toHaveBeenCalledWith('中文');
    expect(callbacks.error).not.toHaveBeenCalled();
    expect(fetchMock.mock.calls[0][0]).toContain('/a%2Fb/stream');
    expect(fetchMock.mock.calls[0][1].headers.Authorization).toBe('Bearer terminal-test-token');
  });

  it('reports disconnect without inventing a successful process exit', async () => {
    fetchMock.mockResolvedValue(response(['data: {"type":"data","data":"hello"}\n\n']));
    const callbacks = receive();
    await vi.waitFor(() => expect(callbacks.error).toHaveBeenCalledWith(expect.stringContaining('status is unknown')));
    expect(callbacks.exit).not.toHaveBeenCalled();
  });

  it('aborts the request on cleanup without firing exit or error', async () => {
    fetchMock.mockImplementation((_url, options) => new Promise((_resolve, reject) => {
      options.signal.addEventListener('abort', () => reject(new DOMException('Aborted', 'AbortError')));
    }));
    const callbacks = receive();
    await vi.waitFor(() => expect(fetchMock).toHaveBeenCalledOnce());
    callbacks.stop();
    expect(fetchMock.mock.calls[0][1].signal.aborted).toBe(true);
    await Promise.resolve();
    expect(callbacks.exit).not.toHaveBeenCalled();
    expect(callbacks.error).not.toHaveBeenCalled();
  });
});
