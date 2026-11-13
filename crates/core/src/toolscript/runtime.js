// ToolScript capability prelude.
//
// This script is evaluated in a fresh global realm before the user body. It
// exposes exactly four globals -- `tools`, `log`, `console`, and `args` -- and
// deletes the raw host bridges. The user body is evaluated as a separate script,
// so it never shares a lexical scope with this closure: rawCall, rawLog, config,
// and every other private binding below are unreachable from user code.
//
// Arguments and results cross the boundary as live JS values and are validated
// in Rust. No user-replaceable intrinsic (JSON.stringify, Object.keys, ...) is
// used for validation, so monkeypatching them cannot alter arguments or bypass
// a limit.
(() => {
  const parse = JSON.parse;
  const config = parse(__toolscript_config);
  const args = parse(__toolscript_args);
  const rawCall = __toolscript_host_call;
  const rawLog = __toolscript_log;
  delete globalThis.__toolscript_host_call;
  delete globalThis.__toolscript_log;
  delete globalThis.__toolscript_config;
  delete globalThis.__toolscript_args;

  const ErrorCtor = Error;
  const StringCtor = String;
  const MAX_CALL_NAME_BYTES = config.maxCallNameBytes;
  const MAX_LOG_BYTES = config.maxLogBytes;

  class ToolScriptError extends ErrorCtor {
    constructor(code, message) {
      super(StringCtor(message));
      this.name = 'ToolScriptError';
      this.code = StringCtor(code);
    }
  }

  // UTF-8 byte length without TextEncoder (QuickJS does not provide one).
  // Valid surrogate pairs are four bytes; lone surrogates count as three.
  function utf8Length(text) {
    let bytes = 0;
    for (let index = 0; index < text.length; index++) {
      const code = text.charCodeAt(index);
      if (code < 0x80) {
        bytes += 1;
      } else if (code < 0x800) {
        bytes += 2;
      } else if (code >= 0xd800 && code <= 0xdbff) {
        const next = index + 1 < text.length ? text.charCodeAt(index + 1) : 0;
        if (next >= 0xdc00 && next <= 0xdfff) {
          bytes += 4;
          index++;
        } else {
          bytes += 3;
        }
      } else {
        bytes += 3;
      }
    }
    return bytes;
  }

  let logBytes = 0;
  let logEntries = 0;
  function log(...parts) {
    let text = '';
    for (let index = 0; index < parts.length; index++) {
      if (index > 0) text += ' ';
      text += StringCtor(parts[index]);
    }
    const bytes = utf8Length(text);
    const separator = logEntries === 0 ? 0 : 1;
    if (logBytes + separator + bytes > MAX_LOG_BYTES) {
      throw new ToolScriptError('log_limit', `log output exceeded the ${MAX_LOG_BYTES}-byte limit`);
    }
    logBytes += separator + bytes;
    logEntries += 1;
    rawLog(text);
  }

  async function callTool(name, value) {
    if (typeof name !== 'string' || name.length === 0) {
      throw new ToolScriptError('invalid_tool_name', 'tools.call requires a non-empty string tool name');
    }
    const nameBytes = utf8Length(name);
    if (nameBytes > MAX_CALL_NAME_BYTES) {
      throw new ToolScriptError(
        'invalid_tool_name',
        `tool name is ${nameBytes} bytes, above the ${MAX_CALL_NAME_BYTES}-byte limit`
      );
    }

    // The raw JS value is handed to Rust; Rust performs the strict JSON walk,
    // depth/cycle checks, and byte limits. There is no JS-side stringify step to
    // hijack.
    const replyText = await rawCall(name, value);
    let reply;
    try {
      reply = parse(replyText);
    } catch (_) {
      throw new ToolScriptError('host_protocol_error', 'tool host returned invalid JSON');
    }
    if (reply === null || typeof reply !== 'object') {
      throw new ToolScriptError('host_protocol_error', 'tool host returned a non-object reply');
    }
    if (reply.ok === true) {
      return reply.value;
    }

    const info = reply.error && typeof reply.error === 'object' ? reply.error : {};
    const error = new ToolScriptError(
      typeof info.code === 'string' ? info.code : 'tool_error',
      typeof info.message === 'string' ? info.message : 'tool call failed'
    );
    error.tool = typeof info.tool === 'string' ? info.tool : name;
    error.callId = typeof info.callId === 'number' ? info.callId : null;
    if (info.details !== undefined) error.details = info.details;
    throw error;
  }

  const tools = Object.freeze({ call: callTool });
  const console = Object.freeze({ log, info: log, warn: log, error: log });

  function expose(name, value) {
    Object.defineProperty(globalThis, name, {
      value,
      writable: false,
      enumerable: true,
      configurable: false,
    });
  }
  expose('tools', tools);
  expose('log', log);
  expose('console', console);
  expose('args', args);
})();
