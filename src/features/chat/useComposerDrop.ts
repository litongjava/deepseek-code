import { useEffect, useRef, useState, type DragEvent } from 'react';

export function useComposerDrop(onFiles: (files: FileList) => void, sessionKey: string | null) {
  const composerRef = useRef<HTMLDivElement>(null);
  const [isDragging, setIsDragging] = useState(false);
  useEffect(() => setIsDragging(false), [sessionKey]);
  useEffect(() => {
    const composer = composerRef.current;
    if (!isDragging || !composer?.animate || window.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches) return;
    // Unit-mass overdamped spring: stiffness 1200, damping 80, roots -20 and -60.
    const frames = Array.from({ length: 31 }, (_, index) => {
      const time = index / 100;
      const displacement = (3 * Math.exp(-20 * time) - Math.exp(-60 * time)) / 2;
      return { transform: `scale(${index === 30 ? 1 : 1 - 0.004 * displacement})` };
    });
    const animation = composer.animate(frames, { duration: 300, easing: 'linear' });
    return () => animation.cancel();
  }, [isDragging]);

  const handleDragOver = (event: DragEvent<HTMLDivElement>) => {
    if (!Array.from(event.dataTransfer.types).includes('Files')) return;
    event.preventDefault();
    event.stopPropagation();
    event.dataTransfer.dropEffect = 'copy';
    setIsDragging(true);
  };
  const handleDragLeave = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.stopPropagation();
    if (event.relatedTarget instanceof Node && event.currentTarget.contains(event.relatedTarget)) return;
    setIsDragging(false);
  };
  const handleDrop = (event: DragEvent<HTMLDivElement>) => {
    event.preventDefault();
    event.stopPropagation();
    setIsDragging(false);
    if (event.dataTransfer.files.length) onFiles(event.dataTransfer.files);
  };
  return { composerRef, isDragging, handleDragOver, handleDragLeave, handleDrop };
}
