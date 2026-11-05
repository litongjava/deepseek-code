import { useEffect, useMemo, useState } from 'react';
import { Pencil, Plus, RotateCcw, Trash2 } from 'lucide-react';
import { customizationApi, type Customization, type CustomizationState, type CustomTool, type PromptMode } from '../api/customization';

const inputClass = 'w-full rounded-lg border border-claude-border bg-claude-bg px-3 py-2 text-sm text-claude-text';
const primaryClass = 'rounded-lg bg-claude-accent px-4 py-2 text-sm text-white disabled:opacity-50';
const secondaryClass = 'rounded-lg border border-claude-border px-3 py-1.5 text-sm text-claude-text hover:bg-claude-hover';
const emptySchema = JSON.stringify({ type: 'object', properties: {} }, null, 2);

interface ToolDraft { index: number | null; name: string; description: string; schema: string; command: string; timeout: string }

const blankTool: ToolDraft = { index: null, name: '', description: '', schema: emptySchema, command: '', timeout: '' };

export function AgentCustomization() {
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

  const tools = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    return (state?.builtin_tools ?? []).filter(tool => !needle || tool.name.toLowerCase().includes(needle));
  }, [state, filter]);

  if (!state || !draft) {
    return error ? <div role="alert" className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">{error}</div> : <p className="text-sm text-claude-textSecondary">加载中…</p>;
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
    try { schema = JSON.parse(toolDraft.schema); } catch { setError('输入参数不是有效的 JSON。'); return; }
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

  const save = async () => {
    setBusy(true); setError(''); setNotice('');
    try {
      await customizationApi.save(draft);
      setState({ ...state, customization: draft });
      setNotice('已保存，下一次对话请求开始生效。');
    } catch (e) { setError(e instanceof Error ? e.message : String(e)); }
    finally { setBusy(false); }
  };

  const dirty = JSON.stringify(draft) !== JSON.stringify(state.customization);
  const replaceEmpty = draft.system_prompt.mode === 'replace' && !draft.system_prompt.text.trim();

  return (
    <div className="mx-auto max-w-3xl space-y-8 text-claude-text">
      <div className="flex items-center justify-between gap-4">
        <div>
          <h2 className="text-xl font-semibold">提示词与工具</h2>
          <p className="mt-1 text-sm text-claude-textSecondary">调整模型收到的系统提示词和可用工具。Desktop 和 dcl 共用这些设置。</p>
        </div>
        <button className={primaryClass} disabled={busy || !dirty || replaceEmpty} onClick={() => void save()}>保存</button>
      </div>
      {error && <div role="alert" className="rounded-lg bg-red-500/10 p-3 text-sm text-red-500">{error}</div>}
      {notice && <div role="status" className="rounded-lg bg-green-500/10 p-3 text-sm text-green-600">{notice}</div>}

      <section className="space-y-3">
        <h3 className="font-semibold">系统提示词</h3>
        <label className="block space-y-1 text-sm"><span>使用方式</span>
          <select aria-label="系统提示词使用方式" className={inputClass} value={draft.system_prompt.mode} onChange={e => {
            const mode = e.target.value as PromptMode;
            // Replacing starts from the built-in text, so it can be edited rather than rewritten.
            setPrompt(mode, mode === 'replace' && !draft.system_prompt.text.trim() ? state.default_system_prompt : draft.system_prompt.text);
          }}>
            <option value="default">使用内置提示词</option>
            <option value="append">在内置提示词后追加</option>
            <option value="replace">完全替换内置提示词</option>
          </select>
        </label>
        {draft.system_prompt.mode === 'default'
          ? <details className="rounded-lg border border-claude-border p-3 text-sm"><summary>查看内置提示词</summary><pre className="mt-2 max-h-80 overflow-auto whitespace-pre-wrap text-xs text-claude-textSecondary">{state.default_system_prompt}</pre></details>
          : <>
            <textarea aria-label="系统提示词内容" rows={12} className={`${inputClass} font-mono`} value={draft.system_prompt.text} onChange={e => setPrompt(draft.system_prompt.mode, e.target.value)} />
            {draft.system_prompt.mode === 'replace' && <>
              <p className="text-xs text-claude-textSecondary">工作目录、操作系统等环境信息仍会自动附加。</p>
              <button className={secondaryClass} onClick={() => setPrompt('replace', state.default_system_prompt)}><RotateCcw size={14} className="mr-1 inline" />恢复为内置内容</button>
            </>}
          </>}
      </section>

      <section className="space-y-3">
        <div className="flex items-center justify-between">
          <h3 className="font-semibold">自定义工具</h3>
          <button className={secondaryClass} onClick={() => setToolDraft({ ...blankTool })}><Plus size={14} className="mr-1 inline" />添加工具</button>
        </div>
        <p className="text-xs text-claude-textSecondary">模型调用时运行你填写的命令：参数以 JSON 传入标准输入和环境变量 TOOL_INPUT，标准输出就是工具结果。命令以当前用户的权限运行，和其他工具一样经过权限审批。</p>
        {draft.custom_tools.map((tool, index) => <div key={tool.name} className="flex items-center gap-3 rounded-lg border border-claude-border p-3">
          <input type="checkbox" aria-label={`启用 ${tool.name}`} checked={tool.enabled} onChange={e => update({ custom_tools: draft.custom_tools.map((item, i) => i === index ? { ...item, enabled: e.target.checked } : item) })} />
          <div className="min-w-0 flex-1"><div className="font-mono text-sm">{tool.name}</div><div className="truncate text-xs text-claude-textSecondary">{tool.description}</div></div>
          <button aria-label={`编辑 ${tool.name}`} className="rounded p-1 hover:bg-claude-hover" onClick={() => setToolDraft({ index, name: tool.name, description: tool.description, schema: JSON.stringify(tool.input_schema, null, 2), command: tool.command, timeout: tool.timeout_secs ? String(tool.timeout_secs) : '' })}><Pencil size={14} /></button>
          <button aria-label={`删除 ${tool.name}`} className="rounded p-1 text-red-500 hover:bg-claude-hover" onClick={() => update({ custom_tools: draft.custom_tools.filter((_, i) => i !== index) })}><Trash2 size={14} /></button>
        </div>)}
        {toolDraft && <form className="space-y-3 rounded-lg border border-claude-border p-3" onSubmit={e => { e.preventDefault(); saveTool(); }}>
          <input aria-label="工具名称" required placeholder="工具名称，例如 lookup_order" className={inputClass} value={toolDraft.name} onChange={e => setToolDraft({ ...toolDraft, name: e.target.value })} />
          <textarea aria-label="工具说明" required rows={2} placeholder="告诉模型这个工具做什么、何时使用" className={inputClass} value={toolDraft.description} onChange={e => setToolDraft({ ...toolDraft, description: e.target.value })} />
          <textarea aria-label="工具命令" required rows={2} placeholder="要运行的命令，例如 python3 tools/lookup.py" className={`${inputClass} font-mono`} value={toolDraft.command} onChange={e => setToolDraft({ ...toolDraft, command: e.target.value })} />
          <textarea aria-label="输入参数 JSON Schema" required rows={6} className={`${inputClass} font-mono`} value={toolDraft.schema} onChange={e => setToolDraft({ ...toolDraft, schema: e.target.value })} />
          <input aria-label="超时秒数" type="number" min={1} max={3600} placeholder="超时秒数（默认 120）" className={inputClass} value={toolDraft.timeout} onChange={e => setToolDraft({ ...toolDraft, timeout: e.target.value })} />
          <div className="flex gap-2"><button className={primaryClass}>确定</button><button type="button" className={secondaryClass} onClick={() => setToolDraft(null)}>取消</button></div>
        </form>}
      </section>

      <section className="space-y-3">
        <h3 className="font-semibold">内置工具</h3>
        <input aria-label="筛选内置工具" placeholder="按名称筛选" className={inputClass} value={filter} onChange={e => setFilter(e.target.value)} />
        <div className="divide-y divide-claude-border rounded-lg border border-claude-border">
          {tools.map(tool => {
            const enabled = !draft.disabled_tools.includes(tool.name);
            const edited = draft.tool_descriptions[tool.name];
            return <div key={tool.name} className="space-y-2 p-3">
              <div className="flex items-center gap-3">
                <input type="checkbox" aria-label={`启用 ${tool.name}`} checked={enabled} onChange={e => toggleTool(tool.name, e.target.checked)} />
                <span className="flex-1 font-mono text-sm">{tool.name}{edited !== undefined && <span className="ml-2 font-sans text-xs text-claude-textSecondary">已修改</span>}</span>
                <button aria-label={`修改 ${tool.name} 的说明`} className="rounded p-1 hover:bg-claude-hover" onClick={() => setEditingTool(editingTool === tool.name ? null : tool.name)}><Pencil size={14} /></button>
              </div>
              {editingTool === tool.name && <div className="space-y-2">
                <textarea aria-label={`${tool.name} 的说明`} rows={8} className={`${inputClass} font-mono text-xs`} value={edited ?? tool.description} onChange={e => setDescription(tool.name, e.target.value)} />
                {edited !== undefined && <button className={secondaryClass} onClick={() => setDescription(tool.name, null)}><RotateCcw size={14} className="mr-1 inline" />恢复默认说明</button>}
              </div>}
            </div>;
          })}
        </div>
      </section>
    </div>
  );
}

export default AgentCustomization;
