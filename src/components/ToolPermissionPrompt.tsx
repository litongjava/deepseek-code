import { useState } from 'react';

export interface ToolPermissionRequest {
  request_id: string;
  tool_use_id: string;
  tool_name: string;
  session_id?: string;
  input?: any;
  allow_session?: boolean;
  toolscript?: { parent_tool_use_id: string; task_id: string };
}
export type ToolPermissionChoice = 'allow' | 'deny' | 'allow_session';

export default function ToolPermissionPrompt({ request, workspace, onRespond, onCancel }: {
  request: ToolPermissionRequest;
  workspace?: string;
  onRespond: (choice: ToolPermissionChoice) => Promise<void>;
  onCancel?: () => Promise<void>;
}) {
  const [pending, setPending] = useState(false);
  const [error, setError] = useState('');
  const respond = async (choice: ToolPermissionChoice | 'cancel') => {
    setPending(true);
    setError('');
    try {
      if (choice === 'cancel') await onCancel?.();
      else await onRespond(choice);
    }
    catch (cause) { setError(cause instanceof Error ? cause.message : String(cause)); }
    finally { setPending(false); }
  };
  const shell = request.tool_name === 'Bash';
  const python = request.tool_name === 'Python';
  const env = python && request.input?.env && typeof request.input.env === 'object'
    ? Object.entries(request.input.env as Record<string, string>)
    : [];
  const saveTarget = python && request.input?.save_name
    ? (() => {
        const separator = workspace?.includes('\\') ? '\\' : '/';
        const base = workspace ? `${workspace.replace(/[\\/]+$/, '')}${separator}temp` : '<workspace>/temp';
        return `${base}${separator}${request.input.save_name.replace(/[\\/]/g, separator)}`;
      })()
    : '';
  return <div className="fixed inset-0 z-[200] flex items-center justify-center bg-black/40">
    <div role="dialog" aria-modal="true" aria-label="Tool Permission Request" className="w-[640px] max-w-[90vw] max-h-[80vh] overflow-auto rounded-2xl border border-deepseek-border bg-deepseek-bg p-5 shadow-xl">
      <h3 className="mb-2 text-[15px] font-semibold text-deepseek-text">{shell ? 'Bash command' : python ? 'Python code' : request.tool_name}</h3>
      {request.toolscript && <p className="mb-2 text-sm text-deepseek-textSecondary">From ToolScript (parent {request.toolscript.parent_tool_use_id})</p>}
      {(shell || python) && request.input?.description && <p className="mb-2 text-sm text-deepseek-textSecondary">{request.input.description}</p>}
      <pre className="mb-3 max-h-[40vh] overflow-auto whitespace-pre-wrap break-words rounded-lg border border-deepseek-border bg-deepseek-input p-3 text-xs text-deepseek-text">{python ? request.input?.code : shell ? request.input?.command : JSON.stringify(request.input, null, 2)}</pre>
      {python && (env.length > 0 || request.input?.cwd || request.input?.interpreter || request.input?.conda || request.input?.save_name) && <dl className="mb-3 space-y-1 text-xs text-deepseek-textSecondary" aria-label="Python execution details">
        {request.input?.interpreter && <div><dt className="inline font-semibold">Interpreter: </dt><dd className="inline font-mono">{request.input.interpreter}</dd></div>}
        {request.input?.conda && <div><dt className="inline font-semibold">Conda environment: </dt><dd className="inline font-mono">{request.input.conda}</dd></div>}
        {request.input?.cwd && <div><dt className="inline font-semibold">Working directory: </dt><dd className="inline font-mono">{request.input.cwd}</dd></div>}
        {request.input?.save_name && <div><dt className="inline font-semibold">Will write: </dt><dd className="inline font-mono">{saveTarget}</dd></div>}
        {env.length > 0 && <div><dt className="font-semibold">Environment:</dt><dd><ul className="font-mono">{env.map(([key, value]) => <li key={key}>{key}={String(value)}</li>)}</ul></dd></div>}
      </dl>}
      {error && <p role="alert" className="mb-3 text-sm text-red-500">{error}</p>}
      <div className="flex flex-wrap justify-end gap-2">
        {onCancel && <button disabled={pending} onClick={() => void respond('cancel')} className="mr-auto rounded-lg border border-deepseek-border px-4 py-1.5 text-sm text-deepseek-text disabled:opacity-50">Cancel turn</button>}
        <button disabled={pending} onClick={() => void respond('deny')} className="rounded-lg border border-deepseek-border px-4 py-1.5 text-sm text-deepseek-text disabled:opacity-50">Deny</button>
        {shell && request.allow_session === true && <button disabled={pending} onClick={() => void respond('allow_session')} className="rounded-lg border border-deepseek-border px-4 py-1.5 text-sm text-deepseek-text disabled:opacity-50">Allow for Session</button>}
        <button disabled={pending} onClick={() => void respond('allow')} className="rounded-lg bg-[#C6613F] px-4 py-1.5 text-sm text-white disabled:opacity-50">Allow</button>
      </div>
    </div>
  </div>;
}
