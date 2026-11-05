import { useState } from 'react';
import ImageLightbox from '../../components/ImageLightbox';
import UserContentBlocks from '../message-list/UserContentBlocks';

function RemoteImage({ url }: { url: string }) {
  const [open, setOpen] = useState(false);
  const [failed, setFailed] = useState(false);
  if (!/^https?:\/\//i.test(url) || failed) return <p role="note">Image unavailable: unsupported URL or failed to load.</p>;
  return <div><button type="button" aria-label="Open tool image" onClick={() => setOpen(true)}><img src={url} alt="Tool attachment" referrerPolicy="no-referrer" onError={() => setFailed(true)} className="max-h-96 max-w-full" /></button>{open && <ImageLightbox src={url} label="Tool attachment" onClose={() => setOpen(false)} />}</div>;
}

/** Only typed blocks are attachments; display-text paths are never file capabilities. */
export default function TypedToolAttachments({ result }: { result: any }) {
  if (!Array.isArray(result?.content)) return null;
  return <div className="space-y-2">{result.content.map((block: any, index: number) => {
    if (block?.type === 'text') return null;
    if (block?.type === 'image' && block.source?.type === 'url' && typeof block.source.url === 'string') return <RemoteImage key={index} url={block.source.url} />;
    if (block?.type === 'image' || block?.type === 'image_url') return <UserContentBlocks key={index} content={[block]} />;
    if (block?.type === 'resource' && typeof block.resource?.text === 'string') return <section key={index} aria-label="Attached resource"><div className="break-all">{block.resource.uri}</div><pre className="max-h-96 overflow-auto whitespace-pre-wrap">{block.resource.text}</pre></section>;
    if (block?.type === 'audio' && typeof block.data === 'string' && /^audio\/[a-z0-9.+-]+$/i.test(block.mimeType)) return <audio key={index} controls preload="none" src={`data:${block.mimeType};base64,${block.data}`} />;
    return <div key={index} role="note">Attachment preview unavailable ({block?.type || 'unknown'}).<pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all">{JSON.stringify(block, null, 2)}</pre></div>;
  })}</div>;
}
