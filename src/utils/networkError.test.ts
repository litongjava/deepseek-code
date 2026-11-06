import { describe, expect, it } from 'vitest';
import { describeError, isNetworkFailure } from './networkError';
import en from '../locales/en.json';

describe('network errors', () => {
  it.each([
    new TypeError('Failed to fetch'),
    new TypeError('network error'),
    'TypeError: network error',
    new TypeError('NetworkError when attempting to fetch resource.'),
    new TypeError('Load failed'),
    'net::ERR_CONNECTION_REFUSED',
  ])('recognizes %s', error => expect(isNetworkFailure(error)).toBe(true));

  it.each(['HTTP 500\nboom', 'Invalid API key', 'Request failed: 404', 'a network error occurred in the model provider response'])(
    'leaves other errors alone: %s', error => expect(isNetworkFailure(error)).toBe(false));

  it('replaces raw browser text with a readable message', () => {
    expect(describeError(new TypeError('Failed to fetch'))).toBe(en.network.unreachable);
    expect(describeError(new Error('Queue paused'))).toBe('Queue paused');
  });
});
