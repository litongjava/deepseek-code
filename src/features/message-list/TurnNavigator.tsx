import { useId, useLayoutEffect, useMemo, useRef, useState, type RefObject } from 'react';
import { extractTextContent } from '../../utils/messageHelpers';
import './TurnNavigator.css';

interface Message {
  id?: string;
  role?: string;
  content?: unknown;
  is_summary?: number;
  is_compact_boundary?: boolean;
}

export function conversationTurns(messages: Message[]) {
  const turns: Array<{ id: string; index: number; prompt: string; response: string }> = [];
  const preview = (content: unknown, limit: number) => extractTextContent(content).replace(/\s+/g, ' ').trim().slice(0, limit);
  messages.forEach((message, index) => {
    if (message.is_summary || message.is_compact_boundary) return;
    if (message.role === 'user') turns.push({ id: message.id || `pending-${index}`, index, prompt: preview(message.content, 80) || '附件消息', response: '' });
    else if (message.role === 'assistant' && turns.length) {
      const turn = turns[turns.length - 1];
      turn.response = [turn.response, preview(message.content, 160)].filter(Boolean).join(' ').slice(0, 160);
    }
  });
  return turns;
}

export default function TurnNavigator({ messages, scrollRef, onNavigate }: {
  messages: Message[];
  scrollRef: RefObject<HTMLDivElement | null>;
  onNavigate: () => void;
}) {
  const turns = useMemo(() => conversationTurns(messages), [messages]);
  const [active, setActive] = useState(0);
  const [preview, setPreview] = useState<number | null>(null);
  const railRef = useRef<HTMLDivElement>(null);
  const previewId = useId();
  // Measure actual message anchors, including layout changes during streaming.
  useLayoutEffect(() => {
    const scroller = scrollRef.current;
    if (!scroller || turns.length < 2) return;
    let frame: number | undefined;
    const update = () => {
      if (frame !== undefined) return;
      frame = requestAnimationFrame(() => {
        frame = undefined;
        const top = scroller.getBoundingClientRect().top + 48;
        let current = 0;
        turns.forEach((turn, index) => {
          const anchor = scroller.querySelector(`[data-message-index="${turn.index}"]`);
          if (anchor && anchor.getBoundingClientRect().top <= top) current = index;
        });
        if (scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 2) current = turns.length - 1;
        setActive(current);
      });
    };
    const observer = new ResizeObserver(update);
    observer.observe(scroller);
    if (scroller.firstElementChild) observer.observe(scroller.firstElementChild);
    scroller.addEventListener('scroll', update, { passive: true });
    update();
    return () => {
      if (frame !== undefined) cancelAnimationFrame(frame);
      observer.disconnect();
      scroller.removeEventListener('scroll', update);
    };
  }, [turns, scrollRef]);

  useLayoutEffect(() => {
    const rail = railRef.current;
    const button = rail?.children[active] as HTMLElement | undefined;
    if (!rail || !button || rail.matches(':hover') || rail.contains(document.activeElement)) return;
    if (button.offsetTop < rail.scrollTop || button.offsetTop + button.offsetHeight > rail.scrollTop + rail.clientHeight) {
      rail.scrollTop = button.offsetTop - rail.clientHeight / 2;
    }
  }, [active]);

  if (turns.length < 2) return null;
  const selected = preview === null ? undefined : turns[preview];
  return <nav aria-label="对话轮次导航" className="turn-navigator" onMouseLeave={() => setPreview(null)}>
    <div className="turn-navigator-marks" ref={railRef} onKeyDown={event => {
      const buttons = [...event.currentTarget.querySelectorAll('button')];
      const index = buttons.indexOf(document.activeElement as HTMLButtonElement);
      const next = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : event.key === 'ArrowDown' ? Math.min(index + 1, buttons.length - 1) : event.key === 'ArrowUp' ? Math.max(index - 1, 0) : null;
      if (next !== null) { event.preventDefault(); buttons[next]?.focus(); }
      if (event.key === 'Escape') setPreview(null);
    }}>
      {turns.map((turn, index) => <button key={turn.id} type="button" aria-label={`跳转到第 ${index + 1} 轮`} aria-current={active === index ? 'true' : undefined}
        aria-describedby={preview === index ? previewId : undefined}
        onMouseEnter={() => setPreview(index)} onFocus={() => setPreview(index)} onBlur={() => setPreview(null)}
        onClick={() => {
          const scroller = scrollRef.current;
          const anchor = scroller?.querySelector(`[data-message-index="${turn.index}"]`);
          if (!scroller || !anchor) return;
          onNavigate();
          scroller.scrollTo({ top: scroller.scrollTop + anchor.getBoundingClientRect().top - scroller.getBoundingClientRect().top - 16, behavior: 'instant' });
          setActive(index);
        }} />)}
    </div>
    {selected && <div role="tooltip" id={previewId} className="turn-navigator-preview">
      <strong>{selected.prompt}</strong>
      {selected.response && <p>{selected.response}</p>}
    </div>}
  </nav>;
}
