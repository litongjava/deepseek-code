export type UserContent = string | Record<string, any>[];

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
