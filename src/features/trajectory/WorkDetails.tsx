import ViewSubagentButton from './ViewSubagentButton';
import TypedToolAttachments from './TypedToolAttachments';
import { memo, useId, useState } from 'react';
import GridDisclosure from '../../components/ui/GridDisclosure';
import { ChevronRight, Code2, Brain } from 'lucide-react';
import { useGeneralSettingsStore, type WorkDetails as DetailLevel } from '../../stores/useGeneralSettingsStore';
import { useWorkspaceModeStore } from '../../stores/useWorkspaceModeStore';
import MarkdownRenderer from '../../components/MarkdownRenderer';
import RequestErrorCard from '../../components/chat/RequestErrorCard';
import ToolDiffView, { shouldUseDiffView } from '../../components/ToolDiffView';
import CopyButton from './CopyButton';
import ToolOutput from './ToolOutput';
import { commandStreams, groupWorkSteps, messageSteps, recordId, toolCategory, toolCommand, toolInput, toolOutput, toolStatus, toolDisplayTitle, rawText, type ProcessStep, type ToolCallGroup, type ToolCallGroupItem, type ToolCallStatus } from './model';

export function InspectButton({ id, onInspect }: { id: string; onInspect?: (id: string) => void }) {
  if (!onInspect) return null;
  return <div className="mt-1 mb-0.5 ml-1 flex items-start"><button type="button" onClick={() => onInspect(id)} className="inline-flex items-center gap-1 rounded-full border-[0.5px] border-claude-border bg-claude-bg px-2 py-0.5 text-[11px] leading-4 text-claude-textSecondary pointer-events-none opacity-0 transition-opacity group-hover/process:pointer-events-auto group-hover/process:opacity-100 group-focus-within/process:pointer-events-auto group-focus-within/process:opacity-100 focus-visible:pointer-events-auto focus-visible:opacity-100" aria-label="Inspect"><Code2 size={13} />Inspect</button></div>;
}
function StatusDot({ status }: { status: string }) {
  const label = status === 'error' ? '运行失败' : status === 'done' ? '运行成功' : status === 'running' ? '运行中' : '已中断';
  return <span role="img" aria-label={label} title={label} className={`h-2 w-2 shrink-0 rounded-full ${status === 'error' ? 'bg-red-500' : status === 'done' ? 'bg-green-500' : status === 'running' ? 'animate-pulse bg-amber-500' : 'bg-claude-textSecondary'}`} />;
}
const dotStatus = (status: ToolCallStatus, interrupted?: boolean) =>
  interrupted ? 'interrupted' : status === 'failed' ? 'error' : status === 'completed' ? 'done' : status === 'running' ? 'running' : 'interrupted';
/** Tree connector for a row inside a group. */
function Connector({ last }: { last: boolean }) {
  return <span aria-hidden="true" className="shrink-0 select-none font-mono text-claude-textSecondary/60">{last ? '└─' : '├─'}</span>;
}
const displayName = (name: string) => name ? name.charAt(0).toUpperCase() + name.slice(1) : 'Tool';
function ToolBlock({ conversationId, messageId, tool, level, live, id, onInspect, last = true }: { conversationId?: string | null; messageId?: string; tool: any; level: DetailLevel; live: boolean; id: string; onInspect?: (id: string) => void; last?: boolean }) {
  const panelId = useId();
  const [expanded, setExpanded] = useState<boolean | null>(null);
  const open = expanded ?? level === 'verbose';
  const input = toolInput(tool);
  const status = toolStatus(tool);
  const failed = status === 'error';
  const running = live && status === 'running';
  const command = toolCommand(tool);
  const output = toolOutput(tool);
  const result = tool.result ?? tool.output ?? tool.error;
  const isCommand = toolCategory(tool.name) === 'Ran commands';
  const streams = isCommand ? commandStreams(result) : null;
  const hasStdout = !!streams?.stdout;
  const hasStderr = !!streams?.stderr;
  const outputLabel = isCommand ? hasStdout && hasStderr ? 'output' : hasStderr ? 'stderr' : 'stdout' : 'Result';
  const outputColor = hasStdout && hasStderr ? 'text-yellow-600 dark:text-yellow-400' : hasStderr ? 'text-red-500' : 'text-claude-textSecondary';
  const title = toolDisplayTitle(tool);
  const description = title !== tool.name ? '' : input.description || input.file_path || input.path || input.pattern || command || tool.name;
  return <div className="group/process relative min-w-0">
    <button type="button" aria-controls={panelId} aria-expanded={open} onClick={() => setExpanded(!open)} className={`flex w-full min-w-0 items-center gap-2 text-left text-[13px] leading-6 ${failed ? 'text-red-500' : 'text-claude-textSecondary'}`}>
      <Connector last={last} /><StatusDot status={status === 'running' && !live ? 'interrupted' : status} />
      <span className="shrink-0 text-claude-text/80">{title !== tool.name ? title : displayName(tool.name)}</span>
      {description && <span className="min-w-0 truncate">· {rawText(description).split('\n')[0]}</span>}
      {running && <span className="ml-auto animate-pulse">运行中</span>}{failed && <span className="ml-auto">失败</span>}
      {status === 'running' && !live && <span className="ml-auto">已中断</span>}
    </button>
    <GridDisclosure open={open} id={panelId}><div className="chat-tool-panel mt-1 overflow-hidden rounded-xl text-xs">
      <div className="flex items-start justify-between gap-3 border-b border-claude-border p-3">
        <pre className="max-h-48 flex-1 overflow-auto whitespace-pre-wrap break-words font-mono">{command || '(无参数)'}</pre>
        {level !== 'compact' && <CopyButton text={command} label="复制命令" />}
      </div>
      {level === 'verbose' && command !== rawText(tool.input) && <div className="border-b border-claude-border p-3"><CopyButton text={rawText(tool.input)} label="复制参数" /><pre className="max-h-64 overflow-auto whitespace-pre-wrap">{rawText(tool.input)}</pre></div>}
      <div className="p-3"><div className="mb-2 flex items-center justify-between"><span className={outputColor}>{outputLabel}</span>{level !== 'compact' && <CopyButton text={output} label="复制输出" />}</div>
        <TypedToolAttachments result={tool.mcp_result} />
        <ToolOutput name={tool.name} result={result} showLabels={hasStdout && hasStderr} empty={running ? '等待输出…' : '(无输出)'} />
      </div>
      {toolCategory(tool.name) !== 'Ran commands' && shouldUseDiffView(tool.name, input) && <ToolDiffView toolName={tool.name} input={input} result={tool.result} />}
      {level === 'verbose' && <div className={`border-t border-claude-border p-3 ${failed ? 'text-red-500' : 'text-claude-textSecondary'}`}>{status} · {tool.startedAt && tool.endedAt ? `${tool.endedAt - tool.startedAt} ms` : '耗时未记录'}</div>}
    </div>
    {status === 'done' && <InspectButton id={id} onInspect={onInspect} />}
    </GridDisclosure>
    {/^(Agent|Task)$/i.test(tool.name) && <ViewSubagentButton conversationId={conversationId} toolId={messageId && tool.id?.startsWith(`${messageId}:`) ? tool.id.slice(messageId.length + 1) : tool.id} live={live} />}
  </div>;
}
/** A reasoning step inside a group: one summary line, full text on demand. */
function ThinkingRow({ step, last }: { step: ProcessStep; last: boolean }) {
  const [open, setOpen] = useState(false);
  const panelId = useId();
  return <div className="min-w-0">
    <button type="button" aria-expanded={open} aria-controls={panelId} onClick={() => setOpen(!open)} className="flex w-full min-w-0 items-center gap-2 text-left text-[13px] leading-6 text-claude-textSecondary">
      <Connector last={last} /><Brain size={13} className="shrink-0" /><span className="shrink-0 text-claude-text/80">Think</span><span className="min-w-0 truncate">· {(step.text || '').trim().split('\n')[0]}</span>
    </button>
    <GridDisclosure open={open} id={panelId}><div className="ml-8 mt-1 max-h-96 overflow-auto whitespace-pre-wrap text-xs text-claude-textSecondary">{step.text}</div></GridDisclosure>
  </div>;
}

/**
 * One run of tool calls and reasoning, shown as a collapsible tree. Rows mount
 * only when the group is opened, so long agent runs stay cheap to render.
 */
const ToolCallGroupView = memo(function ToolCallGroupView({ conversationId, group, message, index, level, live, onInspect }: { conversationId?: string | null; group: ToolCallGroup; message: any; index: number; level: DetailLevel; live: boolean; onInspect?: (id: string) => void }) {
  const panelId = useId();
  // null follows the group: open while it runs, collapsed once finished.
  const [userOpen, setUserOpen] = useState<boolean | null>(null);
  const open = userOpen ?? group.isExpanded;
  const failed = group.status === 'failed';
  const calls = group.items.filter(item => item.tool).length;
  return <div className="chat-tool-run my-1.5" data-group-status={group.status}>
    <button type="button" aria-controls={panelId} aria-expanded={open} onClick={() => setUserOpen(!open)}
      className={`flex max-w-full items-center gap-1.5 py-1 text-left text-[13px] ${failed ? 'text-red-500' : 'text-claude-textSecondary'} hover:text-claude-text`}>
      <ChevronRight size={14} className={`shrink-0 transition-transform ${open ? 'rotate-90' : ''}`} />
      <span>{group.title}</span>
      {calls > 1 && <span className="text-claude-textSecondary/70">· {calls}</span>}
      {group.status === 'running' && <span className="animate-pulse">运行中</span>}
      {failed && <StatusDot status="error" />}
    </button>
    <GridDisclosure open={open} id={panelId}><div className="ml-[7px] space-y-0.5 pl-2">
      {group.items.map((item: ToolCallGroupItem, i: number) => {
        const last = i === group.items.length - 1;
        return item.tool
          ? <ToolBlock messageId={message.id} conversationId={conversationId} key={`${item.step.id}:${level}`} id={recordId(index, item.step.id)} tool={item.tool} level={level} live={live} onInspect={onInspect} last={last} />
          : <ThinkingRow key={item.step.id} step={item.step} last={last} />;
      })}
    </div></GridDisclosure>
  </div>;
});
function ThinkingBlock({ step }: { step: ProcessStep }) {
  const [open, setOpen] = useState(false);
  const panelId = useId();
  return <div className="my-3 text-sm text-claude-textSecondary">
    <button type="button" aria-expanded={open} aria-controls={panelId} onClick={() => setOpen(!open)} className="flex w-full min-w-0 items-center gap-2 text-left">
      <Brain size={14} className="shrink-0" /><span>Think</span><span className="truncate">· {step.text}</span>
    </button>
    <GridDisclosure open={open} id={panelId}><div className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap pl-4">{step.text}</div></GridDisclosure>
  </div>;
}
export default function WorkDetails({ conversationId, message, index, live, onInspect, textOnly = false }: { conversationId?: string | null; message: any; index: number; live: boolean; onInspect?: (id: string) => void; textOnly?: boolean }) {
  const mode = useWorkspaceModeStore(s => s.mode);
  const level = useGeneralSettingsStore(s => s.workDetailsByMode[mode]);
  const coding = useGeneralSettingsStore(s => s.showCodingView);
  const steps = messageSteps(message).filter(step => !textOnly || step.kind === 'text');
  const inspect = coding ? onInspect : undefined;
  return <div className="chat-work-details">
    {groupWorkSteps(message, steps, live).map(item => {
      if (item.kind === 'group') return <ToolCallGroupView conversationId={conversationId} key={item.group.id} group={item.group} message={message} index={index} level={level} live={live} onInspect={inspect} />;
      const s = item.step;
      if (s.kind === 'thinking') return level === 'compact' || level === 'standard' ? null : <ThinkingBlock key={`${s.id}:${level}`} step={s} />;
      return <div key={s.id} className="my-3"><MarkdownRenderer content={s.text || ''} citations={message.citations} isStreaming={live && s === steps[steps.length - 1]} /></div>;
    })}
    {!textOnly && message.requestError && <RequestErrorCard error={message.requestError} />}
  </div>;
}
