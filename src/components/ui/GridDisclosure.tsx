import { useLayoutEffect, useRef, useState, type ReactNode } from 'react';

interface GridDisclosureProps {
  open: boolean;
  id?: string;
  className?: string;
  children: ReactNode;
}

/** Keep expensive content unmounted until first opened, then preserve its state. */
export default function GridDisclosure({ open, id, className = '', children }: GridDisclosureProps) {
  const [visited, setVisited] = useState(open);
  const panel = useRef<HTMLDivElement>(null);
  if (open && !visited) setVisited(true);

  useLayoutEffect(() => {
    if (!open && panel.current?.contains(document.activeElement)) {
      const trigger = Array.from(document.querySelectorAll<HTMLElement>('[aria-controls]'))
        .find(element => element.getAttribute('aria-controls') === id);
      if (trigger) trigger.focus();
      else (document.activeElement as HTMLElement | null)?.blur();
    }
  }, [open, id]);

  return <div ref={panel} id={id} className={`grid-disclosure ${className}`} data-open={open} aria-hidden={!open} inert={!open}>
    <div className="grid-disclosure-inner">{(open || visited) && children}</div>
  </div>;
}
