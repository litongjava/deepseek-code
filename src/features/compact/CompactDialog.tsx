interface CompactDialogProps {
  activeId: string;
  onCompact: (id: string, instruction: string) => Promise<void>;
  compactInstruction: string;
  setCompactInstruction: (v: string) => void;
  setShowCompactDialog: (show: boolean) => void;
}

export default function CompactDialog({
  activeId,
  onCompact,
  compactInstruction,
  setCompactInstruction,
  setShowCompactDialog,
}: CompactDialogProps) {
  const handleCompact = async () => {
    setShowCompactDialog(false);
    if (activeId) await onCompact(activeId, compactInstruction);
  };

  return (
    <div
      className="fixed inset-0 z-[200] flex items-center justify-center bg-black/40"
      onClick={() => setShowCompactDialog(false)}
    >
      <div
        className="bg-deepseek-bg border border-deepseek-border rounded-2xl shadow-xl w-[440px] overflow-hidden"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="px-5 pt-5 pb-3">
          <h3 className="text-[15px] font-semibold text-deepseek-text mb-1">
            Compact conversation
          </h3>
          <p className="text-[13px] text-deepseek-textSecondary leading-snug">
            Summarize the conversation history to free up context space. The
            engine will preserve key decisions and context.
          </p>
        </div>
        <div className="px-5 pb-3">
          <textarea
            className="w-full bg-deepseek-input border border-deepseek-border rounded-lg px-3 py-2 text-[13px] text-deepseek-text placeholder:text-deepseek-textSecondary/50 outline-none focus:border-deepseek-textSecondary/40 transition-colors resize-none"
            rows={3}
            placeholder="Optional: add instructions for the summary (e.g. 'preserve all API endpoint details')"
            value={compactInstruction}
            onChange={(e) => setCompactInstruction(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) {
                e.preventDefault();
                document.getElementById('compact-confirm-btn')?.click();
              }
            }}
            autoFocus
          />
        </div>
        <div className="flex items-center justify-end gap-2 px-5 pb-4">
          <button
            onClick={() => setShowCompactDialog(false)}
            className="px-3.5 py-1.5 text-[13px] text-deepseek-textSecondary hover:text-deepseek-text rounded-lg hover:bg-deepseek-hover transition-colors"
          >
            Cancel
          </button>
          <button
            id="compact-confirm-btn"
            onClick={handleCompact}
            className="px-3.5 py-1.5 text-[13px] text-white bg-[#C6613F] hover:bg-[#D97757] rounded-lg transition-colors font-medium"
          >
            Compact
          </button>
        </div>
      </div>
    </div>
  );
}
