export function planModeFromTool(event: { type?: string; tool_name?: string; content?: string; is_error?: boolean }): boolean | null {
  if (event.type !== 'done' || event.is_error || !['EnterPlanMode', 'ExitPlanMode', 'plan_mode'].includes(event.tool_name || '')) return null;
  try {
    const result = JSON.parse(event.content || '');
    if (result.mode === 'plan') return true;
    if (result.approved === true && ['ask', 'accept_edits', 'bypass'].includes(result.mode)) return false;
  } catch { /* Failed or unsupported output cannot change the displayed mode. */ }
  return null;
}

export function approvedPermissionModeFromTool(event: { type?: string; tool_name?: string; content?: string; is_error?: boolean }): string | null {
  if (planModeFromTool(event) !== false) return null;
  const result = JSON.parse(event.content || '{}');
  return result.mode === 'bypass' ? 'bypass_permissions' : result.mode === 'ask' ? 'ask_permissions' : 'accept_edits';
}
