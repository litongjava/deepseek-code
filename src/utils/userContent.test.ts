import { describe, expect, it } from 'vitest';
import { contentBlocks, embeddedImageUrl, hasUnavailableImages, replaceUserText } from './userContent';
import { extractTextContent } from './messageHelpers';

const image = { type: 'image_url', image_url: { url: 'data:image/png;base64,aGVsbG8=' } };
describe('structured user content', () => {
  it('extracts only ordered text from arrays and historical JSON arrays', () => {
    const content = [{ type: 'text', text: '[Image #3] before' }, image, { type: 'text', text: 'after' }];
    expect(extractTextContent(content)).toBe('[Image #3] before\nafter');
    expect(extractTextContent(JSON.stringify(content))).toBe(extractTextContent(content));
    expect(contentBlocks(JSON.stringify(content))).toEqual(content);
  });
  it('edits text without deleting or changing image bytes', () => {
    const content = [{ type: 'text', text: 'before' }, image, { type: 'text', text: 'after' }, image];
    expect(replaceUserText(content, 'edited')).toEqual([{ type: 'text', text: 'edited' }, image, image]);
    expect(replaceUserText([image], 'new caption')).toEqual([image, { type: 'text', text: 'new caption' }]);
  });
  it('never turns placeholders or arbitrary paths into image URLs', () => {
    for (const url of ['file:///secret.png', 'C:/secret.png', 'https://example.com/image.png', 'data:image/svg+xml;base64,AAAA']) {
      expect(embeddedImageUrl({ type: 'image_url', image_url: { url } })).toBeNull();
    }
    expect(hasUnavailableImages('[Image #65]')).toBe(true);
    expect(hasUnavailableImages([{ type: 'image_unavailable' }])).toBe(true);
    expect(embeddedImageUrl({ type: 'image', source: { type: 'base64', media_type: 'image/png', data: 'aGVsbG8=' } })).toBe(image.image_url.url);
  });
});
