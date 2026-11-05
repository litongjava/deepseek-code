import React, { useState } from 'react';
import ImageLightbox from '../../components/ImageLightbox';
import { contentBlocks, embeddedImageUrl } from '../../utils/userContent';

function EmbeddedImage({ block, label }: { block: Record<string, any>; label: string }) {
  const src = embeddedImageUrl(block);
  const [failed, setFailed] = useState(false);
  const [open, setOpen] = useState(false);
  if (!src || failed) return <div role="note" className="text-sm text-claude-textSecondary">{label} — Image unavailable: saved data is missing or corrupted.</div>;
  return <div className="max-w-[400px]">
    <button type="button" aria-label={`Open ${label}`} onClick={() => setOpen(true)} className="block rounded-xl overflow-hidden border border-claude-border">
      <img src={src} alt={label} onError={() => setFailed(true)} className="w-full h-auto max-h-[400px] object-contain" />
    </button>
    <div className="mt-1 text-xs text-claude-textSecondary">{label}</div>
    {open && <ImageLightbox src={src} label={label} onClose={() => setOpen(false)} />}
  </div>;
}

export default function UserContentBlocks({ content }: { content: unknown }) {
  const blocks = contentBlocks(content) || [];
  const labels = blocks.filter(block => block?.type === 'text').flatMap(block =>
    [...String(block.text || '').matchAll(/\[Image #(\d+)\]/g)].map(match => `Image #${match[1]}`));
  let imageIndex = 0;
  return <div className="max-w-[85%] space-y-3">
    {blocks.map((block, index) => {
      if (block?.type === 'image' || block?.type === 'image_url') {
        const label = labels[imageIndex] || `Image #${imageIndex + 1}`;
        imageIndex += 1;
        return <EmbeddedImage key={index} block={block} label={label} />;
      }
      if (block?.type === 'text' && block.text?.trim()) return <div key={index} className="conversation-text bg-[var(--msg-user-bg)] px-4 py-[10px] text-[15px] whitespace-pre-wrap break-words" style={{ borderRadius: 'var(--msg-user-radius)' }}>{block.text}</div>;
      if (block?.type === 'attachment' && block.source?.type === 'base64' && typeof block.source.data === 'string') return <a
        key={index} download={block.fileName} href={`data:application/octet-stream;base64,${block.source.data}`}
        className="inline-flex max-w-full items-center gap-2 rounded-lg border border-claude-border px-3 py-2 text-sm"
        aria-label={`Download ${block.fileName}`}>
        <span className="truncate">{block.fileName}</span>
        <span className="shrink-0 text-claude-textSecondary">{block.size < 1024 ? `${block.size} B` : `${(block.size / 1024).toFixed(1)} KB`}</span>
      </a>;
      return <div key={index} role="note" className="text-sm text-claude-textSecondary">Image unavailable: saved content is missing or corrupted.</div>;
    })}
    {labels.slice(imageIndex).map((label, index) => <div key={`missing-${index}`} role="note" className="text-sm text-claude-textSecondary">{label} — Image unavailable: this historical message contains only a placeholder.</div>)}
  </div>;
}
