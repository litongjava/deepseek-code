import { useState } from 'react';

export interface ToolPermissionRequest {
  request_id: string;
  tool_use_id: string;
  tool_name: string;
  session_id?: string;
  input?: any;
  allow_session?: boolean;
}
export type ToolPermissionChoice = 'allow' | 'deny' | 'allow_session';

export default function ToolPermissionPrompt({ request, onRespond, onCancel }: {
  request: ToolPermissionRequest;
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
  return <div className="fixed inset-0 z-[200] flex items-center justify-center bg-black/40">
    <div role="dialog" aria-modal="true" aria-label="Tool Permission Request" className="w-[640px] max-w-[90vw] max-h-[80vh] overflow-auto rounded-2xl border border-deepseek-border bg-deepseek-bg p-5 shadow-xl">
      <h3 className="mb-2 text-[15px] font-semibold text-deepseek-text">{shell ? 'Bash command' : request.tool_name}</h3>
      {shell && request.input?.description && <p className="mb-2 text-sm text-deepseek-textSecondary">{request.input.description}</p>}
      <pre className="mb-3 max-h-[40vh] overflow-auto whitespace-pre-wrap break-words rounded-lg border border-deepseek-border bg-deepseek-input p-3 text-xs text-deepseek-text">{shell ? request.input?.command : JSON.stringify(request.input, null, 2)}</pre>
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
