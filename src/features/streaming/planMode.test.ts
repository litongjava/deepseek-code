import { describe, expect, it } from 'vitest';
import { planModeFromTool, approvedPermissionModeFromTool } from './planMode';
describe('plan tool mode updates', () => {
  it('keeps rejected and failed plans in planning mode', () => {
    expect(planModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":false,"mode":"plan"}' })).toBe(true);
    expect(planModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', is_error: true, content: '{"approved":true,"mode":"ask"}' })).toBeNull();
    expect(planModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"mode":"ask"}' })).toBeNull();
  });
  it('accepts only successful supported plan transitions', () => {
    for (const mode of ['ask', 'auto', 'accept_edits', 'bypass']) expect(planModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: JSON.stringify({ approved: true, mode }) })).toBe(false);
    expect(planModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":false,"mode":"bypass"}' })).toBeNull();
  });
});

it('synchronizes the persisted permission selection after approval', () => {
  expect(approvedPermissionModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":true,"mode":"auto"}' })).toBe('auto');
  expect(approvedPermissionModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":true,"mode":"bypass"}' })).toBe('bypass_permissions');
  expect(approvedPermissionModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":true,"mode":"ask"}' })).toBe('ask_permissions');
  expect(approvedPermissionModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":true,"mode":"accept_edits"}' })).toBe('accept_edits');
  expect(approvedPermissionModeFromTool({ type: 'done', tool_name: 'ExitPlanMode', content: '{"approved":false,"mode":"plan"}' })).toBeNull();
});
