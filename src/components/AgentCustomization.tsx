import { useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { Pencil, Plus, RotateCcw, Trash2 } from 'lucide-react';
import { useI18n } from '../hooks/useI18n';
import { customizationApi, type BuiltinTool, type Customization, type CustomizationState, type CustomTool, type PromptMode, type ToolLoading } from '../api/customization';

const inputClass = 'w-full rounded-lg border border-deepseek-border bg-deepseek-bg px-3 py-2 text-sm text-deepseek-text';
const primaryClass = 'rounded-lg bg-deepseek-accent px-4 py-2 text-sm text-white disabled:opacity-50';
const secondaryClass = 'rounded-lg border border-deepseek-border px-3 py-1.5 text-sm text-deepseek-text hover:bg-deepseek-hover';
const emptySchema = JSON.stringify({ type: 'object', properties: {} }, null, 2);

interface ToolDraft { index: number | null; name: string; description: string; schema: string; command: string; timeout: string }

const blankTool: ToolDraft = { index: null, name: '', description: '', schema: emptySchema, command: '', timeout: '' };

/** The two settings pages read the same file; each one edits its own half. */
export type AgentCustomizationSection = 'prompts' | 'tools';

export function AgentCustomization({ section }: { section: AgentCustomizationSection }) {
  const { t } = useI18n();
  const [state, setState] = useState<CustomizationState | null>(null);
  const [draft, setDraft] = useState<Customization | null>(null);
  const [filter, setFilter] = useState('');
  const [editingTool, setEditingTool] = useState<string | null>(null);
  const [toolDraft, setToolDraft] = useState<ToolDraft | null>(null);
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [busy, setBusy] = useState(false);
  const feedbackVisit = useRef(0);

  useEffect(() => {
    customizationApi.load()
      .then(loaded => { setState(loaded); setDraft(loaded.customization); })
      .catch(e => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  // Leaving an entry invalidates its pending save feedback, even on a return visit.
  // Update before paint so a settled request cannot write into the new entry.
  useLayoutEffect(() => {
    feedbackVisit.current += 1;
    setNotice('');
    if (state) setError('');
  }, [section]);

  const tools = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (state?.builtin_tools ?? []).filter(tool => !needle || tool.name.toLowerCase().includes(needle));
  }, [state, filter]);

  if (!state || !draft) {
    return error ? <div role="alert" className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">{error}</div> : <p className="text-sm text-deepseek-textSecondary">{t('agentSettings.loading')}</p>;
  }

  const update = (patch: Partial<Customization>) => { setDraft({ ...draft, ...patch }); setNotice(''); };
  const setPrompt = (mode: PromptMode, text = draft.system_prompt.text) => update({ system_prompt: { mode, text } });
  // The effective switches: an explicit entry, otherwise what the server
  // resolved from the file, so a legacy entry shows its real state.
  const loadingOf = (tool: BuiltinTool): ToolLoading =>
    draft.tool_loading[tool.name] ?? { enabled: tool.enabled, on_demand: tool.on_demand };
  const setLoading = (tool: BuiltinTool, patch: Partial<ToolLoading>) => update({
    tool_loading: { ...draft.tool_loading, [tool.name]: { ...loadingOf(tool), ...patch } },
  });
  const setDescription = (name: string, text: string | null) => {
    const next = { ...draft.tool_descriptions };
    if (text === null) delete next[name]; else next[name] = text;
    update({ tool_descriptions: next });
  };

  const saveTool = () => {
    if (!toolDraft) return;
    let schema: Record<string, unknown>;
    try { schema = JSON.parse(toolDraft.schema); } catch { setError(t('agentSettings.invalidSchema')); return; }
    const previous = toolDraft.index === null ? undefined : draft.custom_tools[toolDraft.index];
    const timeout = Number(toolDraft.timeout);
    const tool: CustomTool = {
      name: toolDraft.name.trim(), description: toolDraft.description.trim(), input_schema: schema,
      command: toolDraft.command, enabled: previous?.enabled ?? true, timeout_secs: toolDraft.timeout.trim() ? timeout : null,
    };
    const list = [...draft.custom_tools];
    if (toolDraft.index === null) list.push(tool); else list[toolDraft.index] = tool;
    update({ custom_tools: list });
    setToolDraft(null);
    setError('');
  };

  // Both entries share one component instance, so the draft also carries edits made
  // on the other entry. Each entry therefore commits, and validates, only its own
  // half; an unsaved prompt must not ride along with a tool save.
  const isPrompts = section === 'prompts';
  const persisted = state.customization;
  const same = (left: unknown, right: unknown) => JSON.stringify(left) === JSON.stringify(right);
  const builtinNames = new Set(state.builtin_tools.map(tool => tool.name));
  // Built-in switches live in `tool_loading`; this list is only an input now.
  const legacy = draft.disabled_tools.filter(name => !builtinNames.has(name));
  const loadingUnchanged = state.builtin_tools.every(tool => {
    const current = loadingOf(tool);
    const stored = persisted.tool_loading[tool.name] ?? { enabled: tool.enabled, on_demand: tool.on_demand };
    return current.enabled === stored.enabled && current.on_demand === stored.on_demand;
  });
  const dirty = isPrompts
    ? !same(draft.system_prompt, persisted.system_prompt)
    : !(loadingUnchanged
      && same(legacy, persisted.disabled_tools)
      && same(draft.tool_descriptions, persisted.tool_descriptions)
      && same(draft.custom_tools, persisted.custom_tools));
  const replaceEmpty = isPrompts && draft.system_prompt.mode === 'replace' && !draft.system_prompt.text.trim();
  const payload: Customization = isPrompts
    ? { ...persisted, system_prompt: draft.system_prompt }
    : {
      ...persisted,
      // A built-in name can no longer sit in both fields.
      disabled_tools: legacy,
      tool_descriptions: draft.tool_descriptions,
      custom_tools: draft.custom_tools,
      tool_loading: draft.tool_loading,
    };

  const save = async () => {
    const visit = feedbackVisit.current;
    setBusy(true); setError(''); setNotice('');
    try {
      const server = await customizationApi.save(payload, isPrompts ? 'prompts' : 'tools');
      // Adopt only the half this entry saved: the other entry's unsaved draft
      // must survive a save made here.
      setState(previous => previous ? {
        ...previous,
        customization: server,
        builtin_tools: previous.builtin_tools.map(tool => {
          const saved = server.tool_loading[tool.name];
          return saved ? { ...tool, enabled: saved.enabled, on_demand: saved.on_demand } : tool;
        }),
      } : previous);
      setDraft(previous => {
        if (!previous) return server;
        return isPrompts
          ? { ...previous, system_prompt: server.system_prompt }
          : {
            ...previous,
            disabled_tools: server.disabled_tools,
            tool_loading: server.tool_loading,
            tool_descriptions: server.tool_descriptions,
            custom_tools: server.custom_tools,
          };
      });
      if (visit === feedbackVisit.current) setNotice(t('agentSettings.saved'));
    } catch (e) {
      if (visit === feedbackVisit.current) setError(e instanceof Error ? e.message : String(e));
    }
    finally { setBusy(false); }
  };

  const promptSettings = (
    <section className="space-y-3">
      <h3 className="font-semibold">{t('agentSettings.systemPrompt')}</h3>
      <label className="block space-y-1 text-sm"><span>{t('agentSettings.mode')}</span>
        <select aria-label={t('agentSettings.modeLabel')} className={inputClass} value={draft.system_prompt.mode} onChange={e => {
          const mode = e.target.value as PromptMode;
          // Replacing starts from the built-in text, so it can be edited rather than rewritten.
          setPrompt(mode, mode === 'replace' && !draft.system_prompt.text.trim() ? state.default_system_prompt : draft.system_prompt.text);
        }}>
          <option value="default">{t('agentSettings.modeDefault')}</option>
          <option value="append">{t('agentSettings.modeAppend')}</option>
          <option value="replace">{t('agentSettings.modeReplace')}</option>
        </select>
      </label>
      {draft.system_prompt.mode === 'default'
        ? <details className="rounded-lg border border-deepseek-border p-3 text-sm"><summary>{t('agentSettings.viewBuiltin')}</summary><pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap text-xs text-deepseek-textSecondary">{state.default_system_prompt}</pre></details>
        : <>
          <textarea aria-label={t('agentSettings.promptText')} rows={12} className={`${inputClass} font-mono`} value={draft.system_prompt.text} onChange={e => setPrompt(draft.system_prompt.mode, e.target.value)} />
          {draft.system_prompt.mode === 'replace' && <>
            <p className="text-xs text-deepseek-textSecondary">{t('agentSettings.environmentNote')}</p>
            <button className={secondaryClass} onClick={() => setPrompt('replace', state.default_system_prompt)}><RotateCcw size={14} className="mr-1 inline" />{t('agentSettings.restoreBuiltin')}</button>
          </>}
        </>}
    </section>
  );

  const toolSettings = <>
    <section className="space-y-3">
      <div className="flex items-center justify-between">
        <h3 className="font-semibold">{t('agentSettings.customTools')}</h3>
        <button className={secondaryClass} onClick={() => setToolDraft({ ...blankTool })}><Plus size={14} className="mr-1 inline" />{t('agentSettings.addTool')}</button>
      </div>
      <p className="text-xs text-deepseek-textSecondary">{t('agentSettings.customToolsNote')}</p>
      {draft.custom_tools.map((tool, index) => <div key={tool.name} className="flex items-center gap-3 rounded-lg border border-deepseek-border p-3">
        <input type="checkbox" aria-label={t('agentSettings.enableTool', { name: tool.name })} checked={tool.enabled} onChange={e => update({ custom_tools: draft.custom_tools.map((item, i) => i === index ? { ...item, enabled: e.target.checked } : item) })} />
        <div className="min-w-0 flex-1"><div className="font-mono text-sm">{tool.name}</div><div className="truncate text-xs text-deepseek-textSecondary">{tool.description}</div></div>
        <button aria-label={t('agentSettings.editTool', { name: tool.name })} className="rounded p-1 hover:bg-deepseek-hover" onClick={() => setToolDraft({ index, name: tool.name, description: tool.description, schema: JSON.stringify(tool.input_schema, null, 2), command: tool.command, timeout: tool.timeout_secs ? String(tool.timeout_secs) : '' })}><Pencil size={14} /></button>
        <button aria-label={t('agentSettings.deleteTool', { name: tool.name })} className="rounded p-1 text-red-500 hover:bg-deepseek-hover" onClick={() => update({ custom_tools: draft.custom_tools.filter((_, i) => i !== index) })}><Trash2 size={14} /></button>
      </div>)}
      {toolDraft && <form className="space-y-3 rounded-lg border border-deepseek-border p-3" onSubmit={e => { e.preventDefault(); saveTool(); }}>
        <input aria-label={t('agentSettings.toolName')} required placeholder={t('agentSettings.toolNamePlaceholder')} className={inputClass} value={toolDraft.name} onChange={e => setToolDraft({ ...toolDraft, name: e.target.value })} />
        <textarea aria-label={t('agentSettings.toolDescription')} required rows={2} placeholder={t('agentSettings.toolDescriptionPlaceholder')} className={inputClass} value={toolDraft.description} onChange={e => setToolDraft({ ...toolDraft, description: e.target.value })} />
        <textarea aria-label={t('agentSettings.toolCommand')} required rows={2} placeholder={t('agentSettings.toolCommandPlaceholder')} className={`${inputClass} font-mono`} value={toolDraft.command} onChange={e => setToolDraft({ ...toolDraft, command: e.target.value })} />
        <textarea aria-label={t('agentSettings.inputSchema')} required rows={6} className={`${inputClass} font-mono`} value={toolDraft.schema} onChange={e => setToolDraft({ ...toolDraft, schema: e.target.value })} />
        <input aria-label={t('agentSettings.timeout')} type="number" min={1} max={3600} placeholder={t('agentSettings.timeoutPlaceholder')} className={inputClass} value={toolDraft.timeout} onChange={e => setToolDraft({ ...toolDraft, timeout: e.target.value })} />
        <div className="flex gap-2"><button className={primaryClass}>{t('agentSettings.confirm')}</button><button type="button" className={secondaryClass} onClick={() => setToolDraft(null)}>{t('agentSettings.cancel')}</button></div>
      </form>}
    </section>

    <section className="space-y-3">
      <h3 className="font-semibold">{t('agentSettings.builtinTools')}</h3>
      <input aria-label={t('agentSettings.filterBuiltin')} placeholder={t('agentSettings.filterPlaceholder')} className={inputClass} value={filter} onChange={e => setFilter(e.target.value)} />
      <div className="divide-y divide-deepseek-border rounded-lg border border-deepseek-border">
        {tools.map(tool => {
          const loading = loadingOf(tool);
          const edited = draft.tool_descriptions[tool.name];
          return <div key={tool.name} className="space-y-2 p-3">
            <div className="flex items-start gap-3">
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-2">
                  <span className="font-mono text-sm">{tool.name}</span>
                  {tool.core && <span className="rounded bg-deepseek-hover px-1.5 py-0.5 text-xs text-deepseek-textSecondary">{t('agentSettings.coreDefault')}</span>}
                  {edited !== undefined && <span className="text-xs text-deepseek-textSecondary">{t('agentSettings.modified')}</span>}
                </div>
                <div className="mt-2 flex flex-wrap items-center gap-x-5 gap-y-1 text-sm">
                  <label className="flex items-center gap-2">
                    <input type="checkbox" aria-label={t('agentSettings.enableTool', { name: tool.name })} checked={loading.enabled} onChange={e => setLoading(tool, { enabled: e.target.checked })} />
                    <span>{t('agentSettings.enable')}</span>
                  </label>
                  <label className="flex items-center gap-2">
                    <input type="checkbox" aria-label={t('agentSettings.onDemandTool', { name: tool.name })} checked={loading.on_demand} onChange={e => setLoading(tool, { on_demand: e.target.checked })} />
                    <span>{t('agentSettings.onDemand')}</span>
                  </label>
                </div>
                <p className="mt-1 text-xs text-deepseek-textSecondary">{t('agentSettings.loadingHint')}</p>
                {tool.name === 'ToolSearch' && !loading.enabled && <p className="mt-1 text-xs text-amber-600">{t('agentSettings.toolSearchDisabledHint')}</p>}
              </div>
              <button aria-label={t('agentSettings.editToolDescription', { name: tool.name })} className="rounded p-1 hover:bg-deepseek-hover" onClick={() => setEditingTool(editingTool === tool.name ? null : tool.name)}><Pencil size={14} /></button>
            </div>
            {editingTool === tool.name && <div className="space-y-2">
              <textarea aria-label={t('agentSettings.descriptionLabel', { name: tool.name })} rows={8} className={`${inputClass} font-mono text-xs`} value={edited ?? tool.description} onChange={e => setDescription(tool.name, e.target.value)} />
              {edited !== undefined && <button className={secondaryClass} onClick={() => setDescription(tool.name, null)}><RotateCcw size={14} className="mr-1 inline" />{t('agentSettings.restoreDescription')}</button>}
            </div>}
          </div>;
        })}
      </div>
    </section>
  </>;

  return (
    <div className="mx-auto max-w-3xl space-y-8 text-deepseek-text">
      <div className="flex items-center justify-between gap-4">
        <div>
          <h2 className="text-xl font-semibold">{isPrompts ? t('customize.prompts') : t('customize.tools')}</h2>
          <p className="mt-1 text-sm text-deepseek-textSecondary">{isPrompts ? t('customize.promptsDescription') : t('customize.toolsDescription')}</p>
          <p className="mt-0.5 text-xs text-deepseek-textSecondary">{t('agentSettings.shared')}</p>
        </div>
        <button className={primaryClass} disabled={busy || !dirty || replaceEmpty} onClick={() => void save()}>{t('agentSettings.save')}</button>
      </div>
      {error && <div role="alert" className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">{error}</div>}
      {notice && <div role="status" className="rounded-lg bg-green-500/10 p-3 text-sm text-green-600">{notice}</div>}

      {isPrompts ? promptSettings : toolSettings}
    </div>
  );
}

export default AgentCustomization;
