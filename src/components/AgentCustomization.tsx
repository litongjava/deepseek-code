import { useEffect, useMemo, useState } from 'react';
import { Pencil, Plus, RotateCcw, Trash2 } from 'lucide-react';
import { useI18n } from '../hooks/useI18n';
import { customizationApi, type Customization, type CustomizationState, type CustomTool, type PromptMode } from '../api/customization';

const inputClass = 'w-full rounded-lg border border-claude-border bg-claude-bg px-3 py-2 text-sm text-claude-text';
const primaryClass = 'rounded-lg bg-claude-accent px-4 py-2 text-sm text-white disabled:opacity-50';
const secondaryClass = 'rounded-lg border border-claude-border px-3 py-1.5 text-sm text-claude-text hover:bg-claude-hover';
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

  useEffect(() => {
    customizationApi.load()
      .then(loaded => { setState(loaded); setDraft(loaded.customization); })
      .catch(e => setError(e instanceof Error ? e.message : String(e)));
  }, []);

  // A save notice belongs to the entry that saved, not to the other one.
  useEffect(() => { setNotice(''); }, [section]);

  const tools = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (state?.builtin_tools ?? []).filter(tool => !needle || tool.name.toLowerCase().includes(needle));
  }, [state, filter]);

  if (!state || !draft) {
    return error ? <div role="alert" className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">{error}</div> : <p className="text-sm text-claude-textSecondary">{t('agentSettings.loading')}</p>;
  }

  const update = (patch: Partial<Customization>) => { setDraft({ ...draft, ...patch }); setNotice(''); };
  const setPrompt = (mode: PromptMode, text = draft.system_prompt.text) => update({ system_prompt: { mode, text } });
  const toggleTool = (name: string, enabled: boolean) => update({
    disabled_tools: enabled ? draft.disabled_tools.filter(item => item !== name) : [...draft.disabled_tools, name],
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
  const dirty = isPrompts
    ? !same(draft.system_prompt, persisted.system_prompt)
    : !(same(draft.disabled_tools, persisted.disabled_tools)
      && same(draft.tool_descriptions, persisted.tool_descriptions)
      && same(draft.custom_tools, persisted.custom_tools));
  const replaceEmpty = isPrompts && draft.system_prompt.mode === 'replace' && !draft.system_prompt.text.trim();
  const payload: Customization = isPrompts
    ? { ...persisted, system_prompt: draft.system_prompt }
    : {
      ...persisted,
      disabled_tools: draft.disabled_tools,
      tool_descriptions: draft.tool_descriptions,
      custom_tools: draft.custom_tools,
    };

  const save = async () => {
    setBusy(true); setError(''); setNotice('');
    try {
      await customizationApi.save(payload);
      // Record what the server now holds; the other entry keeps its own draft.
      setState({ ...state, customization: payload });
      setNotice(t('agentSettings.saved'));
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
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
        ? <details className="rounded-lg border border-claude-border p-3 text-sm"><summary>{t('agentSettings.viewBuiltin')}</summary><pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap text-xs text-claude-textSecondary">{state.default_system_prompt}</pre></details>
        : <>
          <textarea aria-label={t('agentSettings.promptText')} rows={12} className={`${inputClass} font-mono`} value={draft.system_prompt.text} onChange={e => setPrompt(draft.system_prompt.mode, e.target.value)} />
          {draft.system_prompt.mode === 'replace' && <>
            <p className="text-xs text-claude-textSecondary">{t('agentSettings.environmentNote')}</p>
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
      <p className="text-xs text-claude-textSecondary">{t('agentSettings.customToolsNote')}</p>
      {draft.custom_tools.map((tool, index) => <div key={tool.name} className="flex items-center gap-3 rounded-lg border border-claude-border p-3">
        <input type="checkbox" aria-label={t('agentSettings.enableTool', { name: tool.name })} checked={tool.enabled} onChange={e => update({ custom_tools: draft.custom_tools.map((item, i) => i === index ? { ...item, enabled: e.target.checked } : item) })} />
        <div className="min-w-0 flex-1"><div className="font-mono text-sm">{tool.name}</div><div className="truncate text-xs text-claude-textSecondary">{tool.description}</div></div>
        <button aria-label={t('agentSettings.editTool', { name: tool.name })} className="rounded p-1 hover:bg-claude-hover" onClick={() => setToolDraft({ index, name: tool.name, description: tool.description, schema: JSON.stringify(tool.input_schema, null, 2), command: tool.command, timeout: tool.timeout_secs ? String(tool.timeout_secs) : '' })}><Pencil size={14} /></button>
        <button aria-label={t('agentSettings.deleteTool', { name: tool.name })} className="rounded p-1 text-red-500 hover:bg-claude-hover" onClick={() => update({ custom_tools: draft.custom_tools.filter((_, i) => i !== index) })}><Trash2 size={14} /></button>
      </div>)}
      {toolDraft && <form className="space-y-3 rounded-lg border border-claude-border p-3" onSubmit={e => { e.preventDefault(); saveTool(); }}>
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
      <div className="divide-y divide-claude-border rounded-lg border border-claude-border">
        {tools.map(tool => {
          const enabled = !draft.disabled_tools.includes(tool.name);
          const edited = draft.tool_descriptions[tool.name];
          return <div key={tool.name} className="space-y-2 p-3">
            <div className="flex items-center gap-3">
              <input type="checkbox" aria-label={t('agentSettings.enableTool', { name: tool.name })} checked={enabled} onChange={e => toggleTool(tool.name, e.target.checked)} />
              <span className="flex-1 font-mono text-sm">{tool.name}{edited !== undefined && <span className="ml-2 font-sans text-xs text-claude-textSecondary">{t('agentSettings.modified')}</span>}</span>
              <button aria-label={t('agentSettings.editToolDescription', { name: tool.name })} className="rounded p-1 hover:bg-claude-hover" onClick={() => setEditingTool(editingTool === tool.name ? null : tool.name)}><Pencil size={14} /></button>
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
    <div className="mx-auto max-w-3xl space-y-8 text-claude-text">
      <div className="flex items-center justify-between gap-4">
        <div>
          <h2 className="text-xl font-semibold">{isPrompts ? t('customize.prompts') : t('customize.tools')}</h2>
          <p className="mt-1 text-sm text-claude-textSecondary">{isPrompts ? t('customize.promptsDescription') : t('customize.toolsDescription')}</p>
          <p className="mt-0.5 text-xs text-claude-textSecondary">{t('agentSettings.shared')}</p>
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
