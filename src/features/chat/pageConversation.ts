// Return the requested offset only when this transcript owns the key event.
export function conversationPageTop(event: KeyboardEvent, container: HTMLElement): number | null {
  if (event.defaultPrevented || event.isComposing || event.ctrlKey || event.altKey || event.metaKey || event.shiftKey
    || !['PageUp', 'PageDown'].includes(event.key)) return null;
  const target = event.target;
  if (!(target instanceof HTMLElement)
    || target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"]), [role="dialog"]')) return null;
  // Nested editors and scrollable tool results retain their own keyboard behavior.
  for (let node: HTMLElement | null = target; node && node !== container; node = node.parentElement) {
    if (node.scrollHeight > node.clientHeight && /auto|scroll/.test(getComputedStyle(node).overflowY)) return null;
  }
  const step = Math.max(1, Math.floor(container.clientHeight * 4 / 5));
  const direction = event.key === 'PageUp' ? -1 : 1;
  return Math.max(0, Math.min(container.scrollHeight - container.clientHeight, container.scrollTop + direction * step));
}
