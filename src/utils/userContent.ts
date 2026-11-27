export type UserContent = string | Record<string, any>[];
export type SupportedImageMediaType = 'image/png' | 'image/jpeg' | 'image/gif' | 'image/webp';

/** Detect supported image media type from magic bytes, matching Rust's image_media_type_from_bytes. */
export function imageMediaTypeFromBytes(bytes: Uint8Array): SupportedImageMediaType | null {
  if (
    bytes.length >= 8
    && bytes[0] === 0x89 && bytes[1] === 0x50 && bytes[2] === 0x4e && bytes[3] === 0x47
    && bytes[4] === 0x0d && bytes[5] === 0x0a && bytes[6] === 0x1a && bytes[7] === 0x0a
  ) {
    return 'image/png';
  }
  if (bytes.length >= 3 && bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) {
    return 'image/jpeg';
  }
  if (
    bytes.length >= 6
    && bytes[0] === 0x47 && bytes[1] === 0x49 && bytes[2] === 0x46 && bytes[3] === 0x38
    && (bytes[4] === 0x37 || bytes[4] === 0x39) && bytes[5] === 0x61
  ) {
    return 'image/gif';
  }
  if (
    bytes.length >= 12
    && bytes[0] === 0x52 && bytes[1] === 0x49 && bytes[2] === 0x46 && bytes[3] === 0x46
    && bytes[8] === 0x57 && bytes[9] === 0x45 && bytes[10] === 0x42 && bytes[11] === 0x50
  ) {
    return 'image/webp';
  }
  return null;
}

function sniffBase64ImageMediaType(base64: string): SupportedImageMediaType | null {
  try {
    // 16 base64 characters decode to 12 bytes, enough for PNG/JPEG/GIF/WebP signatures.
    const prefix = base64.slice(0, 16);
    if (!prefix) return null;
    const raw = atob(prefix);
    const bytes = new Uint8Array(raw.length);
    for (let i = 0; i < raw.length; i += 1) {
      bytes[i] = raw.charCodeAt(i);
    }
    return imageMediaTypeFromBytes(bytes);
  } catch {
    return null;
  }
}

/**
 * Read an image File as a data URL whose media type matches its actual magic bytes
 * when recognized, so mislabelled extensions (e.g. WebP named .png) remain sendable.
 */
export function readImageDataUrl(
  file: File,
  errorMessage?: string,
): Promise<{ url: string; mediaType: string; data: string }> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => {
      const result = String(reader.result);
      const comma = result.indexOf(',');
      const header = comma >= 0 ? result.slice(0, comma) : '';
      const data = comma >= 0 ? result.slice(comma + 1) : result;
      const declared = header.match(/^data:([^;]+);base64$/i)?.[1] || file.type || '';
      const detected = sniffBase64ImageMediaType(data);
      const mediaType = detected || declared;
      const url = detected ? `data:${detected};base64,${data}` : result;
      resolve({ url, mediaType, data });
    };
    reader.onerror = () => reject(new Error(errorMessage || `Cannot read image: ${file.name}`));
    reader.readAsDataURL(file);
  });
}

export function contentBlocks(content: unknown): Record<string, any>[] | null {
  if (typeof content === 'string' && content.trimStart().startsWith('[')) {
    try { content = JSON.parse(content); } catch { return null; }
  }
  return Array.isArray(content) ? content : null;
}

export function embeddedImageUrl(block: Record<string, any>): string | null {
  const url = block.type === 'image_url' ? block.image_url?.url
    : block.type === 'image' && block.source?.type === 'base64'
      ? `data:${block.source.media_type};base64,${block.source.data}` : null;
  if (typeof url !== 'string' || !/^data:image\/(png|jpeg|gif|webp);base64,[A-Za-z0-9+/]+={0,2}$/.test(url)) return null;
  return url;
}

export function hasUnavailableImages(content: unknown): boolean {
  const blocks = contentBlocks(content);
  if (!blocks) return /\[Image(?:\s+#\d+|:)[^\]]*\]/i.test(typeof content === 'string' ? content : '');
  const placeholders = blocks.filter(block => block?.type === 'text').reduce((count, block) => count + (String(block.text || '').match(/\[Image(?:\s+#\d+|:)[^\]]*\]/gi)?.length || 0), 0);
  const images = blocks.filter(block => block?.type === 'image' || block?.type === 'image_url').length;
  return placeholders > images || blocks.some(block => !block || block.type === 'image_unavailable'
    || ((block.type === 'image' || block.type === 'image_url') && !embeddedImageUrl(block)));
}

/** Change text only; embedded images retain their order and exact bytes. */
export function replaceUserText(content: unknown, text: string): UserContent {
  const blocks = contentBlocks(content);
  if (!blocks) return text;
  let inserted = false;
  const result = blocks.flatMap(block => {
    if (block?.type !== 'text') return [block];
    if (inserted) return [];
    inserted = true;
    return [{ ...block, text }];
  });
  if (!inserted && text) result.push({ type: 'text', text });
  return result;
}
