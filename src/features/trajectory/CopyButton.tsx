import { useEffect, useRef, useState } from 'react';
import { Check, Copy, X } from 'lucide-react';
import { copyToClipboard } from '../../utils/clipboard';

export default function CopyButton({ text, label = '复制' }: { text: string; label?: string }) {
  const [feedback, setFeedback] = useState({ state: 'idle' as 'idle' | 'done' | 'error', sequence: 0 });
  const { state } = feedback;
  const request = useRef(0);
  const button = useRef<HTMLButtonElement>(null);
  useEffect(() => () => { request.current++; }, [text]);
  useEffect(() => {
    if (state !== 'done') return;
    const id = setTimeout(() => setFeedback(previous => ({ ...previous, state: 'idle' })), 1800);
    return () => clearTimeout(id);
  }, [feedback]);
  useEffect(() => {
    const icon = button.current?.querySelector('svg');
    if (state !== 'done' || !icon?.animate || window.matchMedia?.('(prefers-reduced-motion: reduce)').matches) return;
    // Unit-mass spring: stiffness 380, damping 18, sampled for compositor-only feedback.
    const damping = 18 / 2;
    const frequency = Math.sqrt(380 - damping * damping);
    const frames = Array.from({ length: 61 }, (_, index) => {
      const time = index / 100;
      const displacement = Math.exp(-damping * time) * (Math.cos(frequency * time) + damping / frequency * Math.sin(frequency * time));
      return { transform: `scale(${index === 60 ? 1 : 1 - 0.2 * displacement})` };
    });
    const animation = icon.animate(frames, { duration: 600, easing: 'linear' });
    return () => animation.cancel();
  }, [feedback]);
  const feedbackLabel = state === 'done' ? '已复制' : state === 'error' ? '复制失败' : label;
  return <button ref={button} type="button" aria-label={label} title={feedbackLabel} className={`inline-flex shrink-0 items-center justify-center rounded p-1.5 text-xs hover:bg-deepseek-hover focus-visible:ring-2 focus-visible:ring-blue-400 ${state === 'error' ? 'text-red-500' : 'text-deepseek-textSecondary'}`}
    onClick={async event => {
      event.stopPropagation();
      const sequence = ++request.current;
      const success = await copyToClipboard(text);
      if (sequence === request.current) setFeedback({ state: success ? 'done' : 'error', sequence });
    }}>
    {state === 'done' ? <Check size={14} aria-hidden="true" /> : state === 'error' ? <X size={14} aria-hidden="true" /> : <Copy size={14} aria-hidden="true" />}<span className="sr-only" aria-live="polite">{state === 'idle' ? '' : feedbackLabel}</span>
  </button>;
}
