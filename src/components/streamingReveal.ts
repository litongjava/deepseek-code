import { useEffect, useMemo, useState } from 'react';
import type { Element, Root, RootContent, Text } from 'hast';
import type { Plugin } from 'unified';

const MAX_GRAPHEMES = 8;
const STAGGER_MS = 30;
const DURATION_MS = 90;
const WINDOW_MS = (MAX_GRAPHEMES - 1) * STAGGER_MS + DURATION_MS;
const excluded = new Set(['a', 'code', 'pre', 'math', 'svg', 'script', 'style', 'textarea']);
const prose = new Set(['p', 'li', 'h1', 'h2', 'h3', 'h4', 'h5', 'h6', 'td', 'th', 'dt', 'dd']);

interface Batch {
  from: number;
  to: number;
  started: number;
  generation: number;
}

export interface StreamingRevealOptions {
  content: string;
  batch: Batch | null;
}

// Segment the entire text node so an appended combining mark never splits a cluster.
function segments(value: string): Iterable<{ segment: string; index: number }> | null {
  const Segmenter = (Intl as typeof Intl & {
    Segmenter?: new (locale?: string, options?: { granularity: string }) => {
      segment: (text: string) => Iterable<{ segment: string; index: number }>;
    };
  }).Segmenter;
  return Segmenter ? new Segmenter(undefined, { granularity: 'grapheme' }).segment(value) : null;
}

/** Apply after other rehype transforms. Only exact source-mapped prose is eligible. */
export const rehypeStreamingReveal: Plugin<[StreamingRevealOptions], Root> = (options) => (tree) => {
  const { content, batch } = options;
  if (!batch) return;
  const elapsed = Math.max(0, Date.now() - batch.started);
  if (elapsed >= WINDOW_MS) return;
  let count = 0;

  function reveal(node: Text): RootContent[] {
    const start = node.position?.start.offset;
    const end = node.position?.end.offset;
    // Entities, escapes and generated text have no safe one-to-one source mapping.
    if (start === undefined || end === undefined || end <= batch!.from
      || content.slice(start, end) !== node.value) return [node];
    const parts = segments(node.value);
    if (!parts) return [node];
    const result: RootContent[] = [];
    let cursor = 0;
    for (const part of parts) {
      if (count >= MAX_GRAPHEMES) break;
      if (start + part.index < batch!.from || start + part.index + part.segment.length > batch!.to
        || /^\s+$/u.test(part.segment)) continue;
      // Keep the same delay across renders: changing it advances an already-running CSS animation.
      const delay = count++ * STAGGER_MS;
      if (part.index > cursor) result.push({ type: 'text', value: node.value.slice(cursor, part.index) });
      result.push({
        type: 'element',
        tagName: 'span',
        properties: {
          className: ['streaming-reveal-grapheme'],
          style: `animation-name: streaming-reveal-${batch!.generation % 2}; animation-delay: ${delay}ms`,
        },
        children: [{ type: 'text', value: part.segment }],
      });
      cursor = part.index + part.segment.length;
    }
    if (!cursor) return [node];
    if (cursor < node.value.length) result.push({ type: 'text', value: node.value.slice(cursor) });
    return result;
  }

  function walk(parent: Root | Element, eligible: boolean) {
    const children: RootContent[] = [];
    for (const child of parent.children) {
      if (child.type === 'element') {
        const classes = String(child.properties.className ?? '');
        if (!excluded.has(child.tagName) && !/(?:katex|math|streaming-reveal)/i.test(classes)) {
          walk(child, eligible || prose.has(child.tagName));
        }
      }
      children.push(...(child.type === 'text' && eligible && count < MAX_GRAPHEMES ? reveal(child) : [child]));
    }
    parent.children = children as typeof parent.children;
  }
  walk(tree, false);
};

/** Pass the exact string supplied to ReactMarkdown, after any preprocessing. */
export function useStreamingReveal(content: string, isStreaming: boolean) {
  const [reducedMotion, setReducedMotion] = useState(() =>
    typeof window !== 'undefined' && !!window.matchMedia?.('(prefers-reduced-motion: reduce)')?.matches);
  useEffect(() => {
    const media = window.matchMedia?.('(prefers-reduced-motion: reduce)');
    if (!media) return;
    const update = () => setReducedMotion(media.matches);
    update();
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);

  const [state, setState] = useState(() => ({ content, isStreaming, reducedMotion, generation: 0, batch: null as Batch | null }));
  let current = state;
  if (content !== state.content || isStreaming !== state.isStreaming || reducedMotion !== state.reducedMotion) {
    const append = isStreaming && state.isStreaming && !reducedMotion && !state.reducedMotion
      && content.length > state.content.length && content.startsWith(state.content);
    const activeBatch = state.batch && Date.now() - state.batch.started < WINDOW_MS ? state.batch : null;
    const generation = state.generation + (append && activeBatch ? 0 : 1);
    current = {
      content, isStreaming, reducedMotion, generation,
      // Finish the bounded active batch before admitting another; overflow stays plain text.
      batch: append ? (activeBatch ?? { from: state.content.length, to: content.length, started: Date.now(), generation }) : null,
    };
    // Adjust during render: completion/replacement must not paint stale spans for one frame.
    setState(current);
  }
  const batch = current.batch;
  useEffect(() => {
    if (!batch) return;
    const timer = setTimeout(() => {
      setState(previous => previous.batch === batch ? { ...previous, batch: null } : previous);
    }, Math.max(0, WINDOW_MS - (Date.now() - batch.started)));
    return () => clearTimeout(timer);
  }, [batch]);

  return useMemo(() => [rehypeStreamingReveal, { content, batch }] as [typeof rehypeStreamingReveal, StreamingRevealOptions], [content, batch]);
}
