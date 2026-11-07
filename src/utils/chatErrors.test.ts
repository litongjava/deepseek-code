import { describe, expect, it } from 'vitest';
import { formatChatStreamError, parseBadRequest, readChatResponseError } from './chatErrors';

describe('bad request details', () => {
  it.each(['HTTP 400\n{"detail":"Unsupported model"}', 'OpenAI API error 400 Bad Request: unsupported', '400 {"error":"invalid"}', '{"error":{"status":400,"message":"unsupported"}}'])('recognizes status in %s', error => {
    expect(parseBadRequest(error)?.status).toBe(400);
  });
  it.each(['Used 400 tokens', 'HTTP 401\nUnauthorized', 'HTTP 500\nServer error', 'model-400 is unavailable'])('does not classify unrelated numbers: %s', error => {
    expect(parseBadRequest(error)).toBeNull();
  });
  it('retains JSON, text and empty HTTP response bodies', async () => {
    for (const body of ['{"detail":"Unsupported model"}', '<html>Bad request</html>', '']) {
      const error = parseBadRequest(await readChatResponseError(new Response(body, { status: 400 })));
      expect(error).toEqual({ status: 400, detail: body || 'Bad Request' });
    }
  });
  it('retains structured stream errors and status', () => {
    expect(parseBadRequest(formatChatStreamError({ status: 400, error: { message: 'Unsupported model' } })))
      .toEqual({ status: 400, detail: '{"message":"Unsupported model"}' });
  });
});

describe('formatChatError structured diagnostics', () => {
  it('classifies host-generated failures without exposing secrets', async () => {
    const { formatChatError } = await import('./chatErrors');
    const auth =
      'Model API error 401: invalid credential Bearer [REDACTED][stage=response; class=authentication; http=401; attempts=1; retries=0; content_started=unknown]';
    expect(formatChatError(auth)).toContain('API 认证失败');

    const busy =
      'Model request failed: slow down[stage=response; class=rate_limit; http=429; attempts=4; retries=3; content_started=unknown]';
    expect(formatChatError(busy)).toContain('服务暂时繁忙');

    // A transport failure keeps the stage and attempt count for diagnosis.
    const transport =
      'Model request failed: reset by peer[stage=send; class=transient; http=unknown; attempts=4; retries=3; content_started=unknown]';
    const shown = formatChatError(transport);
    expect(shown).toContain('stage=send');
    expect(shown).toContain('attempts=4');
    expect(shown).not.toContain('fixture-');
  });
});

describe('formatChatError network failures', () => {
  it('shows a readable message instead of the browser wording', async () => {
    const { formatChatError } = await import('./chatErrors');
    const en = (await import('../locales/en.json')).default;
    for (const raw of ['Failed to fetch', 'network error', 'Stream error: network error', 'TypeError: network error']) {
      expect(formatChatError(raw)).toBe('⚠️ ' + en.network.unreachable);
    }
    expect(formatChatError('HTTP 500\nboom')).toBe('Error: HTTP 500\nboom');
  });
});
