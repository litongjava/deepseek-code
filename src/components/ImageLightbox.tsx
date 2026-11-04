import React, { useEffect, useRef } from 'react';
import { X } from 'lucide-react';

export default function ImageLightbox({ src, label = 'Image preview', onClose }: { src: string; label?: string; onClose: () => void }) {
  const closeRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    closeRef.current?.focus();
    const keydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') onClose();
      if (event.key === 'Tab') { event.preventDefault(); closeRef.current?.focus(); }
    };
    document.addEventListener('keydown', keydown);
    return () => { document.removeEventListener('keydown', keydown); previous?.focus(); };
  }, [onClose]);
  return <div role="dialog" aria-modal="true" aria-label={label} className="fixed inset-0 z-[100] bg-black/80 flex items-center justify-center backdrop-blur-sm" onClick={onClose}>
    <button ref={closeRef} type="button" aria-label="Close image preview" className="absolute top-4 right-4 text-white/70 hover:text-white bg-black/20 rounded-full p-2" onClick={onClose}><X size={24} /></button>
    <img src={src} alt={label} className="max-w-[95vw] max-h-[95vh] object-contain rounded-lg shadow-2xl" onClick={event => event.stopPropagation()} />
  </div>;
}
