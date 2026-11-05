import { useLayoutEffect, useCallback, useRef, useState } from 'react';
import { conversationPageTop } from './pageConversation';

export function useScrollToBottom(messages: any[], loading: boolean, inputHeight: number, sessionKey?: string | null) {
  const scrollContainerRef = useRef<HTMLDivElement>(null);
  const isAtBottomRef = useRef(true);
  const [scrollbarWidth, setScrollbarWidth] = useState(0);
  const frameRef = useRef<number | null>(null);
  const attachedRef = useRef<HTMLDivElement | null>(null);
  const detachRef = useRef<(() => void) | null>(null);
  const positionRef = useRef({ top: 0, max: 0 });

  const cancelScroll = useCallback(() => {
    if (frameRef.current !== null) cancelAnimationFrame(frameRef.current);
    frameRef.current = null;
  }, []);

  const scrollToBottom = useCallback((behavior: ScrollBehavior = 'auto') => {
    cancelScroll();
    if (!attachedRef.current) return;
    frameRef.current = requestAnimationFrame(() => {
      frameRef.current = null;
      const el = scrollContainerRef.current;
      if (el && el === attachedRef.current && isAtBottomRef.current) {
        el.scrollTo({ top: el.scrollHeight, behavior });
        positionRef.current = { top: el.scrollTop, max: Math.max(0, el.scrollHeight - el.clientHeight) };
      }
    });
  }, [cancelScroll]);

  // Keep the legacy argument for callers; content observation replaces retries.
  const scheduleScrollToBottomAfterRender = useCallback((_attempts = 6) => {
    if (frameRef.current === null && isAtBottomRef.current) scrollToBottom();
  }, [scrollToBottom]);

  const handleScroll = useCallback(() => {
    const el = scrollContainerRef.current;
    if (!el) return;
    const max = Math.max(0, el.scrollHeight - el.clientHeight);
    const previous = positionRef.current;
    const movedUp = el.scrollTop < previous.top;
    const clampedByShrink = max < previous.max && el.scrollTop >= max - 1;
    if (movedUp && !clampedByShrink) {
      isAtBottomRef.current = false;
      cancelScroll();
    } else if (el.scrollTop > previous.top && max - el.scrollTop < 50) {
      isAtBottomRef.current = true;
    }
    // Growth and browser clamping are not user intent; an upward gesture stays paused.
    positionRef.current = { top: el.scrollTop, max };
  }, [cancelScroll]);

  useLayoutEffect(() => {
    cancelScroll();
    isAtBottomRef.current = true;
    const el = scrollContainerRef.current;
    if (el) positionRef.current = { top: el.scrollTop, max: Math.max(0, el.scrollHeight - el.clientHeight) };
    scheduleScrollToBottomAfterRender();
  }, [sessionKey, cancelScroll, scheduleScrollToBottomAfterRender]);

  // Object refs preserve the public API and are checked on every commit because
  // the transcript container is conditional (home / chat / trajectory).
  useLayoutEffect(() => {
    const el = scrollContainerRef.current;
    if (attachedRef.current === el) return;
    detachRef.current?.();
    attachedRef.current = el;
    if (!el) return;
    let touchY: number | undefined;
    positionRef.current = { top: el.scrollTop, max: Math.max(0, el.scrollHeight - el.clientHeight) };
    const stopFollowing = () => {
      isAtBottomRef.current = false;
      cancelScroll();
      el.scrollTo({ top: el.scrollTop, behavior: 'instant' });
    };
    const wheel = (event: WheelEvent) => { if (event.deltaY < 0) stopFollowing(); };
    const keydown = (event: KeyboardEvent) => {
      const top = conversationPageTop(event, el);
      if (top !== null) {
        event.preventDefault();
        if (event.key === 'PageUp') stopFollowing();
        else cancelScroll();
        el.scrollTo({ top, behavior: 'instant' });
        handleScroll();
        return;
      }
      if (event.defaultPrevented || ['PageUp', 'PageDown'].includes(event.key)) return;
      const target = event.target as HTMLElement;
      if (target.closest('input, textarea, select, [contenteditable="true"]')) return;
      if (['ArrowUp', 'PageUp', 'Home'].includes(event.key) || (event.key === ' ' && event.shiftKey)) stopFollowing();
    };
    const touchstart = (event: TouchEvent) => { touchY = event.touches[0]?.clientY; };
    const touchmove = (event: TouchEvent) => {
      const nextY = event.touches[0]?.clientY;
      if (touchY !== undefined && nextY !== undefined && nextY > touchY) stopFollowing();
      touchY = nextY;
    };
    const scroll = handleScroll;
    const update = () => {
      setScrollbarWidth(el.offsetWidth - el.clientWidth);
      scheduleScrollToBottomAfterRender();
    };
    const observer = new ResizeObserver(update);
    const observeContent = () => {
      observer.disconnect();
      observer.observe(el);
      for (const child of el.children) observer.observe(child);
      update();
    };
    const mutations = new MutationObserver(observeContent);
    mutations.observe(el, { childList: true });
    observeContent();
    el.addEventListener('wheel', wheel, { passive: true });
    el.addEventListener('keydown', keydown);
    el.addEventListener('touchstart', touchstart, { passive: true });
    el.addEventListener('touchmove', touchmove, { passive: true });
    el.addEventListener('scroll', scroll, { passive: true });
    detachRef.current = () => {
      cancelScroll();
      observer.disconnect();
      mutations.disconnect();
      el.removeEventListener('wheel', wheel);
      el.removeEventListener('keydown', keydown);
      el.removeEventListener('touchstart', touchstart);
      el.removeEventListener('touchmove', touchmove);
      el.removeEventListener('scroll', scroll);
    };
  });

  useLayoutEffect(() => {
    scheduleScrollToBottomAfterRender();
  }, [messages, loading, inputHeight, scheduleScrollToBottomAfterRender]);

  useLayoutEffect(() => () => {
    detachRef.current?.();
    detachRef.current = null;
    attachedRef.current = null;
    cancelScroll();
  }, [cancelScroll]);

  return { scrollContainerRef, isAtBottomRef, scrollbarWidth, scrollToBottom, scheduleScrollToBottomAfterRender, handleScroll };
}
