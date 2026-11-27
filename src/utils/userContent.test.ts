import { describe, expect, it } from 'vitest';
import { contentBlocks, embeddedImageUrl, hasUnavailableImages, imageMediaTypeFromBytes, readImageDataUrl, replaceUserText } from './userContent';
import { buildContentBlocks, extractTextContent } from './messageHelpers';
import type { PendingFile } from '../components/FileUploadPreview';

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

  it('sniffs real image media types from magic bytes and rewrites mislabelled files', async () => {
    const png = new Uint8Array([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
    const jpeg = new Uint8Array([0xff, 0xd8, 0xff, 0xe0]);
    const gif = new Uint8Array([0x47, 0x49, 0x46, 0x38, 0x39, 0x61]);
    const webp = new Uint8Array([0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50]);

    expect(imageMediaTypeFromBytes(png)).toBe('image/png');
    expect(imageMediaTypeFromBytes(jpeg)).toBe('image/jpeg');
    expect(imageMediaTypeFromBytes(gif)).toBe('image/gif');
    expect(imageMediaTypeFromBytes(webp)).toBe('image/webp');
    expect(imageMediaTypeFromBytes(new Uint8Array([0x89, 0x50, 0x4e, 0x47]))).toBeNull();
    expect(imageMediaTypeFromBytes(new TextEncoder().encode('plain text'))).toBeNull();

    const mislabelled = new File([webp], '2.png', { type: 'image/png' });
    const read = await readImageDataUrl(mislabelled);
    expect(read.mediaType).toBe('image/webp');
    expect(read.url).toBe('data:image/webp;base64,UklGRgAAAABXRUJQ');

    const blocks = await buildContentBlocks('look', [{
      file: mislabelled,
      fileType: 'image',
      mimeType: 'image/png',
      fileName: '2.png',
      status: 'done',
    } as PendingFile]);
    expect(blocks[0]).toEqual({
      type: 'image',
      source: {
        type: 'base64',
        media_type: 'image/webp',
        data: 'UklGRgAAAABXRUJQ',
      },
    });
  });
});
