import React, { useState, useEffect, useCallback, useRef } from 'react';
import { useNavigate } from 'react-router-dom';
import {
  ArrowLeft, Plus, Search, Trash2, Sparkles, LayoutGrid, FileText,
  ChevronRight, ChevronDown, Folder, File, Info, Eye, Code,
  Settings, Check, MessageSquare, ClipboardList, Upload, Github, X, FolderPlus, Server, Puzzle, Wrench } from 'lucide-react';
import AgentCustomization from './AgentCustomization';
import MarkdownRenderer, { CodeBlock } from './MarkdownRenderer';
import { getSkills, getSkillDetail, getSkillFile, createSkill, updateSkill, deleteSkill, toggleSkill, getGithubStatus, getGithubAuthUrl, disconnectGithub } from '../api';
import searchIconImg from '../assets/icons/search-icon.png';
import skillsImg from '../assets/icons/skills.png';
import connectorsImg from '../assets/icons/connectors.png';
import customizeIconImg from '../assets/icons/customize-icon.png';
import customizeMainImg from '../assets/icons/customize-main.png';
import { useI18n } from '../hooks/useI18n';
import McpSettingsPage from './McpSettingsPage';
import PluginsPage from './PluginsPage';
import GithubTokenForm from '../features/github/GithubTokenForm';

interface Skill {
  id: string;
  name: string;
  description: string;
  content?: string;
  isExample?: boolean;
  sourceDir?: string;
  source?: string;
  user_id?: string | null;
  enabled: boolean;
  createdAt?: string;
  files?: any[];
}

interface FileTreeNodeProps {
  skill: Skill;
  isExpanded: boolean;
  onExpand: (e: React.MouseEvent) => void;
  selectedSkillId: string | null;
  detail: Skill | null;
  selectSkill: (id: string) => void;
  selectedFile: string;
  setSelectedFile: (name: string) => void;
  setSelectedSkillId: (id: string) => void;
  loadFileContent: (skillId: string, filePath: string) => void;
  t: (key: string) => string;
}

const FileTreeNode: React.FC<FileTreeNodeProps> = ({
  skill, isExpanded, onExpand,
  selectedSkillId, detail, selectSkill,
  selectedFile, setSelectedFile, setSelectedSkillId, loadFileContent, t
}) => {
  const isSelected = selectedSkillId === skill.id;
  const hasFiles = detail && detail.id === skill.id && detail.files && detail.files.length > 0;
  const isEnabled = skill.enabled;
  const [mockFolderState, setMockFolderState] = useState<Record<string, boolean>>({});

  const toggleMockFolder = (name: string, e: React.MouseEvent) => {
    e.stopPropagation();
    setMockFolderState(prev => ({ ...prev, [name]: !prev[name] }));
  };

  return (
    <div className="select-none overflow-hidden">
      <div
        onClick={(e) => {
          if (isSelected && detail?.id === skill.id) {
            onExpand(e);
          } else {
            selectSkill(skill.id);
          }
        }}
        className={`flex items-center gap-3 px-3 py-2 rounded-lg cursor-pointer transition-colors ${isSelected ? 'bg-deepseek-hover' : 'hover:bg-deepseek-hover/50'}`}
      >
        <div className="flex items-center justify-center w-[24px] h-[24px] rounded-md border border-deepseek-border bg-black/5 dark:bg-[#30302E] flex-shrink-0">
          <img
            src={skillsImg}
            className={`w-[15.5px] h-[15.5px] ${isEnabled ? 'dark:invert opacity-90' : 'dark:invert opacity-30 invert-[0.3]'}`}
            alt="skill"
          />
        </div>

        <div className="flex-1 min-w-0">
          <span className={`truncate text-[14px] ${isEnabled ? 'text-deepseek-text font-medium' : 'text-[#262624] dark:text-[#6B6B68]'}`}>
            {skill.name}
          </span>
        </div>

        {isSelected && (
          <ChevronRight size={16} className={`text-deepseek-textSecondary transition-transform duration-300 ease-[cubic-bezier(0.4,0,0.2,1)] ${isExpanded && hasFiles ? 'rotate-90' : ''}`} />
        )}
      </div>

      <div className={`grid transition-all duration-300 ease-[cubic-bezier(0.4,0,0.2,1)] ${isExpanded && hasFiles ? 'grid-rows-[1fr] opacity-100' : 'grid-rows-[0fr] opacity-0 pointer-events-none'}`}>
        <div className="overflow-hidden">
          <div className="pl-6 pt-0.5 pb-2 space-y-0.5">
            {(detail?.files || []).map((file: any) => (
              <div key={file.name}>
                <div
                  onClick={(e) => {
                    e.stopPropagation();
                    if (file.type === 'folder') toggleMockFolder(file.name, e);
                    else {
                      setSelectedFile(file.name);
                      setSelectedSkillId(skill.id);
                      loadFileContent(skill.id, file.name);
                    }
                  }}
                  className={`flex items-center gap-2 px-2 py-1.5 rounded-md cursor-pointer text-[13.5px] group ${selectedSkillId === skill.id && selectedFile === file.name && file.type !== 'folder' ? 'bg-deepseek-hover' : 'hover:bg-deepseek-hover/70'}`}
                >
                  <div className="w-[12px]" />
                  {file.type === 'folder' ? (
                    <Folder size={15.5} className="text-deepseek-textSecondary fill-deepseek-textSecondary/10" />
                  ) : (
                    <File size={15.5} className="text-deepseek-textSecondary group-hover:text-deepseek-text transition-colors" />
                  )}
                  <span className={`truncate ${selectedSkillId === skill.id && selectedFile === file.name && file.type !== 'folder' ? 'text-deepseek-text font-medium' : 'text-deepseek-textSecondary group-hover:text-deepseek-text transition-colors'}`}>{file.name}</span>
                  {file.type === 'folder' && <ChevronRight size={14} className={`ml-auto text-deepseek-textSecondary transition-transform duration-200 ${mockFolderState[file.name] ? 'rotate-90' : ''}`} />}
                </div>
                {file.type === 'folder' && (
                  <div className={`grid transition-all duration-300 ease-[cubic-bezier(0.4,0,0.2,1)] ${mockFolderState[file.name] ? 'grid-rows-[1fr] opacity-100' : 'grid-rows-[0fr] opacity-0'}`}>
                    <div className="overflow-hidden">
                      {file.children && file.children.length > 0 ? (
                        <div className="pl-6 py-0.5 space-y-0.5">
                          {file.children.map((child: any) => (
                            <div
                              key={child.name}
                              onClick={(e) => {
                                e.stopPropagation();
                                const childPath = file.name + '/' + child.name;
                                setSelectedFile(child.name);
                                setSelectedSkillId(skill.id);
                                loadFileContent(skill.id, childPath);
                              }}
                              className={`flex items-center gap-2 px-2 py-1 rounded-md cursor-pointer text-[13px] group ${selectedSkillId === skill.id && selectedFile === child.name ? 'bg-deepseek-hover' : 'hover:bg-deepseek-hover/70'}`}
                            >
                              <File size={14} className="text-deepseek-textSecondary group-hover:text-deepseek-text transition-colors" />
                              <span className={`truncate ${selectedSkillId === skill.id && selectedFile === child.name ? 'text-deepseek-text font-medium' : 'text-deepseek-textSecondary group-hover:text-deepseek-text transition-colors'}`}>{child.name}</span>
                            </div>
                          ))}
                        </div>
                      ) : (
                        <div className="pl-12 py-1 pb-2 text-[12px] text-deepseek-textSecondary/60 italic pointer-events-none">
                          {t('customize.emptyDirectory')}
                        </div>
                      )}
                    </div>
                  </div>
                )}
              </div>
            ))}
          </div>
        </div>
      </div>
    </div>
  );
};

const CustomizePage = ({ onCreateWithDeepSeek, onBack }: { onCreateWithDeepSeek?: () => void; onBack?: () => void }) => {
  const { t } = useI18n();
  const navigate = useNavigate();
  const [tab, setTab] = useState<'overview' | 'skills' | 'mcp' | 'connectors' | 'plugins' | 'prompts' | 'tools'>('overview');
  const [examples, setExamples] = useState<Skill[]>([]);
  const [mySkills, setMySkills] = useState<Skill[]>([]);
  const [selectedSkillId, setSelectedSkillId] = useState<string | null>(null);
  const [detail, setDetail] = useState<Skill | null>(null);
  const [search, setSearch] = useState('');
  const [loading, setLoading] = useState(true);
  const [skillError, setSkillError] = useState('');
  const selectionRequest = useRef(0);
  const [editing, setEditing] = useState(false);
  const entries = [
    { id: 'skills', label: t('customize.skills'), description: t('customize.teachClaude'), image: skillsImg },
    { id: 'mcp', label: 'MCP', description: t('settings.mcpServers'), icon: Server },
    { id: 'connectors', label: t('customize.connectors'), description: t('customize.integrateTools'), image: connectorsImg },
    { id: 'plugins', label: t('customize.plugins'), description: t('customize.pluginsDescription'), icon: Puzzle },
    { id: 'prompts', label: t('customize.prompts'), description: t('customize.promptsDescription'), icon: FileText },
    { id: 'tools', label: t('customize.tools'), description: t('customize.toolsDescription'), icon: Wrench },
  ] as const;

  const [githubConnected, setGithubConnected] = useState(false);
  // Without an OAuth client secret on the Bridge, connect with a token instead.
  const [githubOauth, setGithubOauth] = useState(true);
  const [githubUser, setGithubUser] = useState<{ login: string; avatar_url: string; name?: string } | null>(null);
  const [selectedConnector, setSelectedConnector] = useState<'github' | 'gdrive'>('github');

  useEffect(() => {
    getGithubStatus().then(data => {
      setGithubConnected(data.connected);
      setGithubOauth(data.oauthAvailable !== false);
      if (data.user) setGithubUser(data.user);
    }).catch(() => { });
    const poll = setInterval(() => {
      getGithubStatus().then(data => {
        if (data.connected && !githubConnected) {
          setGithubConnected(true);
          if (data.user) setGithubUser(data.user);
        }
      }).catch(() => { });
    }, 3000);
    return () => clearInterval(poll);
  }, [githubConnected]);

  const handleGithubConnect = async () => {
    try {
      const { url } = await getGithubAuthUrl();
      import('../utils/tauriAPI').then(m => {
        m.tauriAPI.openExternal(url);
      }).catch(() => window.open(url, '_blank'));
    } catch (e) { console.error('GitHub auth error:', e); }
  };

  const handleGithubDisconnect = async () => {
    await disconnectGithub();
    setGithubConnected(false);
    setGithubUser(null);
  };

  const [expandedSections, setExpandedSections] = useState<Set<string>>(new Set(['examples', 'myskills']));
  const [expandedSkills, setExpandedSkills] = useState<Set<string>>(new Set());
  const [selectedFile, setSelectedFile] = useState<string>('SKILL.md');
  const [fileContent, setFileContent] = useState<string>('');
  const [showSearchInput, setShowSearchInput] = useState(false);

  const [showPlusMenu, setShowPlusMenu] = useState(false);
  const [showUploadModal, setShowUploadModal] = useState(false);

  const [creating, setCreating] = useState(false);
  const [editName, setEditName] = useState('');
  const [editDesc, setEditDesc] = useState('');
  const [editContent, setEditContent] = useState('');
  const [saving, setSaving] = useState(false);
  const [viewMode, setViewMode] = useState<'preview' | 'code'>('preview');

  useEffect(() => {
    if (selectedFile) {
      if (selectedFile.endsWith('.md') || selectedFile.endsWith('.mdx')) {
        setViewMode('preview');
      } else {
        setViewMode('code');
      }
    }
  }, [selectedFile]);

  const fetchList = useCallback(async () => {
    setLoading(true);
    setSkillError('');
    try {
      const data = await getSkills();
      if (!Array.isArray(data.skills)) throw new Error(data.error || 'Invalid skills response');
      const skills: Skill[] = data.skills;
      setExamples(skills.filter(skill => skill.source === 'bundled' || skill.isExample));
      setMySkills(skills.filter(skill => skill.source !== 'bundled' && !skill.isExample));
    } catch (e) { setSkillError(String(e)); }
    finally { setLoading(false); }
  }, []);

  useEffect(() => { if (tab === 'skills') void fetchList(); }, [tab, fetchList]);

  const loadFileContent = async (skillId: string, filePath: string) => {
    const request = ++selectionRequest.current;
    setSkillError('');
    setFileContent('');
    if (filePath === 'SKILL.md') return;
    try {
      const data = await getSkillFile(skillId, filePath);
      if (request === selectionRequest.current) setFileContent(data.content || '');
    } catch (e) {
      if (request === selectionRequest.current) setSkillError(String(e));
    }
  };

  const selectSkill = async (id: string) => {
    const request = ++selectionRequest.current;
    setSkillError('');
    setCreating(false);
    setEditing(false);
    setDetail(null);
    setSelectedSkillId(id);
    setSelectedFile('SKILL.md');
    setFileContent('');
    setExpandedSkills(prev => { const s = new Set(prev); s.delete(id); return s; });
    try {
      const d = await getSkillDetail(id);
      if (request !== selectionRequest.current) return;
      setDetail(d);
      if (!d.isExample) {
        setEditName(d.name);
        setEditDesc(d.description || '');
        setEditContent(d.content || '');
      }
      setExpandedSkills(prev => new Set([...prev, id]));
    } catch (e) { if (request === selectionRequest.current) setSkillError(String(e)); }
  };

  const handleToggle = async (id: string, current: boolean, e?: React.MouseEvent) => {
    e?.stopPropagation();
    setSkillError('');
    try {
      await toggleSkill(id, !current);
      setExamples(prev => prev.map(s => s.id === id ? { ...s, enabled: !current } : s));
      setMySkills(prev => prev.map(s => s.id === id ? { ...s, enabled: !current } : s));
      setDetail(prev => prev?.id === id ? { ...prev, enabled: !current } : prev);
      const updated = await getSkillDetail(id);
      setDetail(prev => prev?.id === id ? updated : prev);
      if (selectedSkillId === id) setEditContent(updated.content || '');
    } catch (e) { setSkillError(String(e)); }
  };

  const toggleSection = (section: string) => {
    const newSet = new Set(expandedSections);
    if (newSet.has(section)) newSet.delete(section);
    else newSet.add(section);
    setExpandedSections(newSet);
  };

  const toggleSkillExpand = (id: string, e: React.MouseEvent) => {
    e.stopPropagation();
    const newSet = new Set(expandedSkills);
    if (newSet.has(id)) newSet.delete(id);
    else newSet.add(id);
    setExpandedSkills(newSet);
  };

  const handleCreate = async () => {
    if (!editName.trim()) return;
    setSaving(true);
    setSkillError('');
    try {
      const s = await createSkill({ name: editName, description: editDesc, content: editContent });
      setMySkills(prev => [s, ...prev]);
      setCreating(false);
      setSelectedSkillId(s.id);
      setDetail(s);
      setEditContent(s.content || '');
    } catch (e) { setSkillError(String(e)); }
    setSaving(false);
  };

  const handleSave = async () => {
    if (!detail) return;
    setSaving(true);
    setSkillError('');
    try {
      await updateSkill(detail.id, { name: editName, description: editDesc, content: editContent });
      await fetchList();
      await selectSkill(detail.id);
    } catch (e) { setSkillError(String(e)); }
    setSaving(false);
  };

  const handleDelete = async () => {
    if (!detail) return;
    if (!confirm(t('customize.confirmDeleteSkill'))) return;
    try {
      await deleteSkill(detail.id);
      setMySkills(prev => prev.filter(s => s.id !== detail.id));
      setSelectedSkillId(null);
      setDetail(null);
    } catch (e) { setSkillError(String(e)); }
  };

  const startCreate = () => {
    ++selectionRequest.current;
    setSkillError('');
    setEditing(false);
    setCreating(true);
    setSelectedSkillId(null);
    setDetail(null);
    setEditName('');
    setEditDesc('');
    setEditContent('');
  };

  const q = search.toLowerCase();
  const filteredExamples = examples.filter(s => s.name.toLowerCase().includes(q));
  const filteredMy = mySkills.filter(s => s.name.toLowerCase().includes(q));

  const ToggleSwitch = ({ enabled, onToggle, size = 'md' }: { enabled: boolean; onToggle: (e: React.MouseEvent) => void, size?: 'sm' | 'md' }) => (
    <button role="switch" aria-checked={enabled} aria-label={enabled ? t('customize.disable') : t('customize.enable')} onClick={onToggle}
      className={`relative inline-flex items-center rounded-full transition-colors duration-300 ease-[cubic-bezier(0.4,0,0.2,1)] ${enabled ? 'bg-blue-600' : 'bg-gray-200 dark:bg-gray-600'} ${size === 'sm' ? 'h-4 w-7' : 'h-5 w-9'}`}>
      <span className={`inline-block rounded-full bg-white shadow-sm transition-all duration-300 ease-[cubic-bezier(0.4,0,0.2,1)] ${size === 'sm' ? 'h-3 w-3' : 'h-4 w-4'} ${enabled ? (size === 'sm' ? 'translate-x-[14px]' : 'translate-x-[18px]') : 'translate-x-[2px]'}`} />
    </button>
  );

  return (
    <div className="flex h-full min-h-0 w-full overflow-hidden bg-deepseek-bg text-deepseek-text font-sans">

      {/* 1. Left Navigation (Fixed width) */}
      <div className="w-[160px] xl:w-[200px] 2xl:w-[240px] border-r border-deepseek-border flex flex-col pt-4 pb-4 flex-shrink-0 bg-deepseek-bg">
        <div className="px-4 mb-6">
          <button onClick={() => {
            if (tab !== 'overview') setTab('overview');
            else if (onBack) onBack();
            else navigate(-1);
          }}
            className="flex items-center gap-2 text-deepseek-text font-medium hover:text-deepseek-text/80 transition-colors">
            <ArrowLeft size={20} />
            <span className="text-lg font-semibold">{t('customize.title')}</span>
          </button>
        </div>
        <nav aria-label={t('customize.title')} className="flex-1 px-2 space-y-1 overflow-y-auto">
          {entries.map(item => (
            <button key={item.id} aria-current={tab === item.id ? 'page' : undefined} onClick={() => setTab(item.id)}
              className={`w-full flex items-center gap-3 px-3 py-2 text-[15px] font-medium rounded-lg transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 ${tab === item.id ? 'bg-deepseek-hover text-deepseek-text' : 'text-deepseek-text hover:bg-deepseek-hover'}`}>
              {'image' in item ? <img src={item.image} alt="" className="w-[22px] h-[22px] dark:invert" /> : <item.icon size={22} />}
              {item.label}
            </button>
          ))}
        </nav>
      </div>

      {/* 2. Middle Column: Skills List or Connectors List */}
      {tab === 'skills' ? (
        <div className="w-[220px] 2xl:w-[300px] border-r border-deepseek-border flex flex-col flex-shrink-0 bg-deepseek-bg">
          {/* Header */}
          <div className="h-14 px-4 flex items-center justify-between border-b border-deepseek-border">
            <span className="font-semibold text-deepseek-text">{t('customize.skills')}</span>
            <div className="flex items-center gap-2 relative">
              <button
                aria-label={t('customize.filterSkills')}
                onClick={() => setShowSearchInput(!showSearchInput)}
                className="p-1.5 rounded-md hover:bg-deepseek-hover text-deepseek-textSecondary hover:text-deepseek-text transition-colors group"
              >
                <Search size={21} className="opacity-70 group-hover:opacity-100 transition-opacity" />
              </button>
              <div className="relative">
                <button
                  aria-label={t('customize.createNewSkills')}
                  onClick={() => setShowPlusMenu(!showPlusMenu)}
                  className="p-1.5 rounded-md hover:bg-deepseek-hover text-deepseek-textSecondary hover:text-deepseek-text transition-colors"
                >
                  <Plus size={22} />
                </button>
                {showPlusMenu && (
                  <>
                    <div className="fixed inset-0 z-40" onClick={() => setShowPlusMenu(false)} />
                    <div className="absolute top-full right-0 mt-2 w-64 bg-white dark:bg-[#202020] rounded-[16px] shadow-[0_4px_24px_rgba(0,0,0,0.15)] border border-deepseek-border py-2 z-50">
                      <button className="w-full flex items-center gap-3.5 px-4 py-3 text-[14.5px] font-medium text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors text-left" onClick={() => { setShowPlusMenu(false); onCreateWithDeepSeek?.(); }}>
                        <MessageSquare size={18} className="text-deepseek-textSecondary" />
                        {t('customize.createWithDeepSeek')}
                      </button>
                      <button className="w-full flex items-center gap-3.5 px-4 py-3 text-[14.5px] font-medium text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors text-left" onClick={() => { setShowPlusMenu(false); startCreate(); }}>
                        <ClipboardList size={18} className="text-deepseek-textSecondary" />
                        {t('customize.writeSkillInstructions')}
                      </button>
                      <button className="w-full flex items-center gap-3.5 px-4 py-3 text-[14.5px] font-medium text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors text-left" onClick={() => { setShowPlusMenu(false); setShowUploadModal(true); }}>
                        <Upload size={18} className="text-deepseek-textSecondary" />
                        {t('customize.uploadSkill')}
                      </button>
                    </div>
                  </>
                )}
              </div>
            </div>
          </div>

          {/* Search Input (Conditional) */}
          {showSearchInput && (
            <div className="px-3 py-2 border-b border-deepseek-border">
              <input
                autoFocus
                value={search}
                onChange={e => setSearch(e.target.value)}
                placeholder={t('customize.filterSkills')}
                className="w-full px-2 py-1.5 bg-deepseek-input rounded-md text-sm outline-none border border-transparent focus:border-blue-500"
              />
            </div>
          )}

          {/* List Content */}
          <div className="flex-1 overflow-y-auto py-2">
            {/* Examples Section */}
            {filteredExamples.length > 0 && (
              <div className="mb-2">
                <button
                  onClick={() => toggleSection('examples')}
                  className="w-full flex items-center gap-1 px-3 py-1.5 text-xs font-semibold text-deepseek-textSecondary hover:text-deepseek-text uppercase tracking-wider"
                >
                  <ChevronDown size={14} className={`transition-transform ${expandedSections.has('examples') ? '' : '-rotate-90'}`} />
                  {t('customize.examples')}
                </button>

                {expandedSections.has('examples') && (
                  <div className="mt-0.5 px-2 space-y-0.5">
                    {filteredExamples.map(skill => (
                      <FileTreeNode
                        key={skill.id}
                        skill={skill}
                        isExpanded={expandedSkills.has(skill.id)}
                        onExpand={(e) => toggleSkillExpand(skill.id, e)}
                        selectedSkillId={selectedSkillId}
                        detail={detail}
                        selectSkill={selectSkill}
                        selectedFile={selectedFile}
                        setSelectedFile={setSelectedFile}
                        setSelectedSkillId={setSelectedSkillId}
                        loadFileContent={loadFileContent}
                        t={t}
                      />
                    ))}
                  </div>
                )}
              </div>
            )}

            {/* My Skills Section */}
            {filteredMy.length > 0 && (
              <div>
                <button
                  onClick={() => toggleSection('myskills')}
                  className="w-full flex items-center gap-1 px-3 py-1.5 text-xs font-semibold text-deepseek-textSecondary hover:text-deepseek-text uppercase tracking-wider"
                >
                  <ChevronDown size={14} className={`transition-transform ${expandedSections.has('myskills') ? '' : '-rotate-90'}`} />
                  {t('customize.mySkills')}
                </button>

                {expandedSections.has('myskills') && (
                  <div className="mt-0.5 px-2 space-y-0.5 overflow-hidden">
                    <div className={`grid transition-all duration-300 ease-[cubic-bezier(0.4,0,0.2,1)] grid-rows-[1fr] opacity-100`}>
                      <div className="overflow-hidden">
                        {filteredMy.map(skill => (
                          <FileTreeNode
                            key={skill.id}
                            skill={skill}
                            isExpanded={expandedSkills.has(skill.id)}
                            onExpand={(e) => toggleSkillExpand(skill.id, e)}
                            selectedSkillId={selectedSkillId}
                            detail={detail}
                            selectSkill={selectSkill}
                            selectedFile={selectedFile}
                            setSelectedFile={setSelectedFile}
                            setSelectedSkillId={setSelectedSkillId}
                            loadFileContent={loadFileContent}
                            t={t}
                          />
                        ))}
                      </div>
                    </div>
                  </div>
                )}
              </div>
            )}

            {loading && <p role="status" className="p-4 text-sm text-deepseek-textSecondary">{t('common.loading')}</p>}
            {!loading && !skillError && !filteredExamples.length && !filteredMy.length && (
              <div className="p-4 text-center text-sm text-deepseek-textSecondary">
                {t('customize.noSkillsFound')}
              </div>
            )}
          </div>
        </div>
      ) : tab === 'connectors' ? (
        <div className="w-[220px] 2xl:w-[300px] border-r border-deepseek-border flex flex-col flex-shrink-0 bg-deepseek-bg">
          <div className="h-14 px-4 flex items-center justify-between border-b border-deepseek-border">
            <span className="font-semibold text-deepseek-text">{t('customize.connectors')}</span>
          </div>
          <div className="flex-1 overflow-y-auto pt-4 px-2">
            {githubConnected && (
              <div className="mb-2">
                <button className="w-full flex items-center gap-1.5 px-3 py-1.5 text-[12px] font-medium text-deepseek-textSecondary/80 uppercase tracking-wider">
                  <ChevronDown size={14} className="stroke-[2px] opacity-70" />
                  {t('customize.connected')}
                </button>
                <div className="mt-1 px-1 space-y-1">
                  <div
                    onClick={() => setSelectedConnector('github')}
                    className={`flex items-center gap-3 px-3 py-2.5 rounded-[10px] cursor-pointer transition-colors ${selectedConnector === 'github' ? 'bg-deepseek-hover' : 'hover:bg-deepseek-hover/50'}`}
                  >
                    <Github size={20} className="fill-current stroke-none opacity-80" />
                    <span className="truncate text-[14px] text-deepseek-text font-medium">{t('customize.githubIntegration')}</span>
                    <div className="ml-auto w-2 h-2 rounded-full bg-green-500 flex-shrink-0" />
                  </div>
                </div>
              </div>
            )}
            {(!githubConnected || true) && (
              <div className="mb-2">
                <button className="w-full flex items-center gap-1.5 px-3 py-1.5 text-[12px] font-medium text-deepseek-textSecondary/80 uppercase tracking-wider">
                  <ChevronDown size={14} className="stroke-[2px] opacity-70" />
                  {githubConnected ? t('customize.available') : t('customize.notConnected')}
                </button>
                <div className="mt-1 px-1 space-y-1">
                  {!githubConnected && (
                    <div
                      onClick={() => setSelectedConnector('github')}
                      className={`flex items-center gap-3 px-3 py-2.5 rounded-[10px] cursor-pointer transition-colors ${selectedConnector === 'github' ? 'bg-deepseek-hover' : 'hover:bg-deepseek-hover/50'}`}
                    >
                      <Github size={20} className="fill-current stroke-none opacity-80" />
                      <span className={`truncate text-[14px] ${selectedConnector === 'github' ? 'text-deepseek-text font-medium' : 'text-deepseek-textSecondary'}`}>{t('customize.githubIntegration')}</span>
                    </div>
                  )}
                  <div
                    onClick={() => setSelectedConnector('gdrive')}
                    className={`flex items-center gap-3 px-3 py-2.5 rounded-[10px] cursor-pointer transition-colors ${selectedConnector === 'gdrive' ? 'bg-deepseek-hover' : 'hover:bg-deepseek-hover/50'}`}
                  >
                    <div className="w-5 h-5 flex items-center justify-center">
                      <img src="https://upload.wikimedia.org/wikipedia/commons/1/12/Google_Drive_icon_%282020%29.svg" className="w-[18px] h-[18px]" alt="" />
                    </div>
                    <span className={`truncate text-[14px] ${selectedConnector === 'gdrive' ? 'text-deepseek-text font-medium' : 'text-deepseek-textSecondary'}`}>Google Drive</span>
                  </div>
                </div>
              </div>
            )}
          </div>
        </div>
      ) : null}

      {/* 3. Right Column: Detail / Create / Overview */}
      <div className="flex-1 flex flex-col min-w-0">
        {tab === 'skills' && skillError && <div role="alert" className="m-4 p-3 rounded-lg border border-red-500/30 text-red-500 text-sm">
          {skillError}
          <button onClick={fetchList} className="ml-3 underline">{t('customize.refresh')}</button>
        </div>}
        {tab === 'mcp' ? (
          <div className="flex-1 min-h-0 overflow-y-auto p-6 lg:p-8"><McpSettingsPage /></div>
        ) : tab === 'plugins' ? (
          <div className="flex-1 min-h-0 overflow-y-auto p-6 lg:p-8"><PluginsPage /></div>
        ) : tab === 'prompts' ? (
          <div className="flex-1 min-h-0 overflow-y-auto p-6 lg:p-8"><AgentCustomization section="prompts" /></div>
        ) : tab === 'tools' ? (
          <div className="flex-1 min-h-0 overflow-y-auto p-6 lg:p-8"><AgentCustomization section="tools" /></div>
        ) : tab === 'overview' ? (
          <div className="flex flex-col items-center h-full w-full overflow-y-auto max-w-2xl mx-auto px-6 py-12">
            <div className="mb-6">
              <img src={customizeMainImg} alt="Customize" className="w-[140px] h-auto dark:invert opacity-90" />
            </div>
            <div className="text-center mb-12">
              <h2 className="text-xl font-medium text-deepseek-text mb-2">{t('customize.manageContext')}</h2>
            </div>

            <div aria-label={t('customize.manageContext')} className="w-full space-y-4">
              {entries.map(item => (
                <button key={item.id} onClick={() => setTab(item.id)}
                  className="w-full min-h-[96px] flex items-center gap-4 p-4 border border-deepseek-border bg-black/[0.02] dark:bg-white/[0.04] rounded-[24px] hover:bg-black/[0.05] dark:hover:bg-white/[0.07] focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-blue-500 focus-visible:ring-offset-2 focus-visible:ring-offset-claude-bg transition-colors text-left group">
                  <span className="w-11 h-11 shrink-0 rounded-xl bg-deepseek-bg/50 border border-deepseek-border flex items-center justify-center text-deepseek-textSecondary group-hover:border-deepseek-textSecondary/30 transition-colors">
                    {'image' in item ? <img src={item.image} className="w-7 h-7 dark:invert opacity-70" alt="" /> : <item.icon size={28} />}
                  </span>
                  <span className="flex-1 min-w-0">
                    <span className="block font-medium text-deepseek-text text-[15.5px]">{item.label}</span>
                    <span className="block text-sm text-deepseek-textSecondary">{item.description}</span>
                  </span>
                  <ChevronRight size={16} className="shrink-0 text-deepseek-textSecondary" />
                </button>
              ))}
            </div>
          </div>
        ) : tab === 'connectors' ? (
          <div className="flex-1 h-full bg-deepseek-bg overflow-y-auto">
            {selectedConnector === 'github' ? (
              githubConnected ? (
                <div className="h-full flex flex-col">
                  <div className="flex items-center justify-between px-8 py-5 border-b border-deepseek-border">
                    <div className="flex items-center gap-3">
                      <Github size={22} className="fill-current stroke-none text-deepseek-text" />
                      <span className="text-[17px] font-semibold text-deepseek-text">{t('customize.githubIntegration')}</span>
                    </div>
                    <button onClick={handleGithubDisconnect} className="px-4 py-1.5 text-[13px] font-medium text-deepseek-textSecondary border border-deepseek-border rounded-lg hover:bg-deepseek-hover transition-colors">
                      {t('customize.disconnect')}
                    </button>
                  </div>
                  <div className="px-8 py-6 overflow-y-auto">
                    <p className="text-[14px] text-deepseek-textSecondary mb-6 leading-relaxed">
                      {t('customize.connectedTo')} <span className="text-deepseek-text font-medium">{githubUser?.login}</span>{t('customize.canAccessRepo')}
                    </p>
                    <ul className="space-y-4 text-[14px] text-deepseek-textSecondary leading-relaxed">
                      <li>
                        <span className="text-deepseek-text font-medium">{t('customize.conversations')}</span> — {t('customize.quoteRepo')}
                      </li>
                      <li>
                        <span className="text-deepseek-text font-medium">{t('customize.projectsTab')}</span> — {t('customize.syncFiles')}
                      </li>
                      <li>
                        <span className="text-deepseek-text font-medium">{t('customize.code')}</span> — {t('customize.browseCode')}
                      </li>
                      <li>
                        <span className="text-deepseek-text font-medium">{t('customize.more')}</span> — {t('customize.moreFeatures')}
                      </li>
                    </ul>
                  </div>
                </div>
              ) : (
                <div className="flex flex-col items-center justify-center h-full mt-[-10vh]">
                  <div className="w-[68px] h-[68px] bg-black/[0.03] dark:bg-white/[0.04] border border-deepseek-border rounded-[18px] flex items-center justify-center mb-6 shadow-sm">
                    <Github size={36} className="text-deepseek-text fill-current stroke-none" />
                  </div>
                  <p className="text-[14px] text-deepseek-textSecondary mb-6 font-normal">{t('customize.notConnectedGithub')}</p>
                  {githubOauth ? (
                    <button
                      onClick={handleGithubConnect}
                      className="px-6 py-2.5 text-[14px] font-medium text-deepseek-bg bg-deepseek-text hover:opacity-90 rounded-[10px] transition-colors shadow-sm"
                    >
                      {t('customize.connect')}
                    </button>
                  ) : (
                    <GithubTokenForm onConnected={user => { setGithubConnected(true); setGithubUser(user); }} />
                  )}
                </div>
              )
            ) : (
              <div className="flex flex-col items-center mt-[-10vh]">
                <div className="w-[68px] h-[68px] bg-black/[0.03] dark:bg-white/[0.04] border border-deepseek-border rounded-[18px] flex items-center justify-center mb-6 shadow-sm">
                  <img src="https://upload.wikimedia.org/wikipedia/commons/1/12/Google_Drive_icon_%282020%29.svg" className="w-9 h-9" alt="" />
                </div>
                <p className="text-[14px] text-deepseek-textSecondary mb-6 font-normal">{t('customize.comingSoon')}</p>
              </div>
            )}
          </div>
        ) : creating || editing ? (
          // Create Form
          <div className="max-w-3xl mx-auto w-full p-8 space-y-6 overflow-y-auto">
            <h2 className="text-2xl font-semibold text-deepseek-text">{t(editing ? 'customize.editDetails' : 'customize.createSkillTitle')}</h2>
            <div className="space-y-4">
              <div>
                <label className="block text-sm font-medium text-deepseek-textSecondary mb-1.5">{t('customize.name')}</label>
                <input
                  aria-label={t('customize.name')} value={editName} onChange={e => setEditName(e.target.value)}
                  className="w-full px-3 py-2 rounded-lg border border-deepseek-border bg-transparent text-deepseek-text outline-none focus:border-blue-500 transition-colors"
                  placeholder={t('customize.skillNamePlaceholder')}
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-deepseek-textSecondary mb-1.5">{t('customize.description')}</label>
                <input
                  aria-label={t('customize.description')} value={editDesc} onChange={e => setEditDesc(e.target.value)}
                  className="w-full px-3 py-2 rounded-lg border border-deepseek-border bg-transparent text-deepseek-text outline-none focus:border-blue-500 transition-colors"
                  placeholder={t('customize.skillDescPlaceholder')}
                />
              </div>
              <div>
                <label className="block text-sm font-medium text-deepseek-textSecondary mb-1.5">{t('customize.content')}</label>
                <textarea
                  aria-label={t('customize.content')} value={editContent} onChange={e => setEditContent(e.target.value)} rows={15}
                  className="w-full px-3 py-2 rounded-lg border border-deepseek-border bg-transparent text-deepseek-text font-mono text-sm outline-none focus:border-blue-500 transition-colors resize-y"
                  placeholder={t('customize.contentPlaceholder')}
                />
              </div>
              <div className="flex gap-3 pt-2">
                <button onClick={editing ? handleSave : handleCreate} disabled={saving || !editName.trim()}
                  className="px-4 py-2 rounded-lg bg-blue-600 text-white font-medium hover:bg-blue-700 disabled:opacity-50 transition-colors">
                  {saving ? t('common.loading') : t(editing ? 'customize.save' : 'customize.createSkillBtn')}
                </button>
                <button onClick={() => { setCreating(false); setEditing(false); }}
                  className="px-4 py-2 rounded-lg border border-deepseek-border text-deepseek-text hover:bg-deepseek-hover transition-colors">
                  {t('customize.cancel')}
                </button>
              </div>
            </div>
          </div>
        ) : detail ? (
          // Detail View
          <div className="flex flex-col h-full overflow-hidden">
            {/* Header */}
            <div className="flex-shrink-0 px-8 py-6 border-b border-transparent">
              <div className="flex items-start justify-between mb-4">
                <h2 className="text-xl font-bold text-deepseek-text">{detail.name}</h2>
                <div className="flex items-center gap-4">
                  <ToggleSwitch
                    enabled={detail.enabled}
                    onToggle={(e) => handleToggle(detail.id, detail.enabled, e)}
                  />
                  {!detail.isExample && detail.source !== 'bundled' && <button onClick={() => setEditing(true)} className="text-sm text-deepseek-textSecondary hover:text-deepseek-text">
                    {t('customize.editDetails')}
                  </button>}
                </div>
              </div>

              <div className="space-y-4">
                {detail.isExample && (
                  <div className="text-sm">
                    <span className="text-deepseek-textSecondary">{t('customize.addedBy')}</span>
                    <div className="font-medium text-deepseek-text mt-0.5">Anthropic</div>
                  </div>
                )}

                <div>
                  <div className="flex items-center gap-1.5 text-sm text-deepseek-textSecondary mb-1">
                    {t('customize.description')}
                    <Info size={14} />
                  </div>
                  <p className="text-sm text-deepseek-text leading-relaxed">
                    {detail.description || t('customize.noDescription')}
                  </p>
                </div>
              </div>
            </div>

            {/* Content Area */}
            <div className="flex-1 overflow-y-auto px-8 pb-8">
              <div className="border border-deepseek-border rounded-xl bg-white dark:bg-[#30302E] overflow-hidden shadow-sm">
                <div className="flex items-center justify-end px-4 py-2 border-b border-deepseek-border bg-deepseek-bg/50">
                  <button
                    onClick={() => setViewMode(viewMode === 'preview' ? 'code' : 'preview')}
                    className="flex items-center gap-2 px-3 py-1.5 text-xs font-medium text-deepseek-textSecondary hover:bg-deepseek-hover rounded-md transition-colors"
                  >
                    {viewMode === 'preview' ? <Code size={14} /> : <Eye size={14} />}
                    {viewMode === 'preview' ? t('customize.codeView') : t('customize.previewView')}
                  </button>
                </div>

                <div className="p-6 min-h-[400px]">
                  {viewMode === 'preview' ? (
                    <div className="prose prose-sm max-w-none dark:prose-invert">
                      <MarkdownRenderer content={selectedFile === 'SKILL.md' ? (detail.content || '') : (fileContent || '')} />
                    </div>
                  ) : (
                    <div className="mx-auto w-full">
                      <CodeBlock
                        className="!border-transparent !bg-transparent !my-0"
                        language={
                          selectedFile.endsWith('.py') ? 'python' :
                            selectedFile.endsWith('.js') || selectedFile.endsWith('.jsx') ? 'javascript' :
                              selectedFile.endsWith('.ts') || selectedFile.endsWith('.tsx') ? 'typescript' :
                                selectedFile.endsWith('.css') ? 'css' :
                                  selectedFile.endsWith('.html') ? 'html' :
                                    selectedFile.endsWith('.json') ? 'json' :
                                      selectedFile.endsWith('.sh') ? 'bash' : 'text'
                        }
                        code={selectedFile === 'SKILL.md' ? (detail.content || '') : (fileContent || '')}
                      />
                    </div>
                  )}
                </div>
              </div>
            </div>
          </div>
        ) : (
          <div className="flex items-center justify-center h-full text-deepseek-textSecondary">
            <div className="text-center">
              <Sparkles size={32} className="mx-auto mb-3 opacity-20" />
              <p>{t('customize.selectSkillPrompt')}</p>
            </div>
          </div>
        )}
      </div>

      {showUploadModal && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/60 backdrop-blur-sm p-4">
          <div className="bg-[#242424] w-[460px] rounded-[16px] flex flex-col shadow-[0_10px_40px_rgba(0,0,0,0.5)] relative border border-white/10 overflow-hidden">
            {/* Header */}
            <div className="flex items-center justify-between px-6 pt-6 pb-4">
              <h3 className="text-[16px] font-semibold text-white tracking-wide">{t('customize.uploadSkillTitle')}</h3>
              <button
                onClick={() => setShowUploadModal(false)}
                className="text-white/50 hover:text-white/80 transition-colors"
              >
                <X size={18} strokeWidth={2} />
              </button>
            </div>

            {/* Dropzone */}
            <div className="mx-6 mb-6 p-8 border border-dashed border-white/20 rounded-xl flex flex-col items-center justify-center bg-white/[0.02] cursor-pointer hover:bg-white/[0.05] transition-colors group">
              <FolderPlus size={22} className="text-white/50 group-hover:text-white/70 transition-colors mb-3 stroke-[1.5]" />
              <div className="text-[13px] text-white/50 group-hover:text-white/70 transition-colors">
                {t('customize.dragDrop')}
              </div>
            </div>

            {/* Requirements Details */}
            <div className="px-6 pb-8">
              <div className="text-[12px] font-medium text-white/60 mb-2.5">{t('customize.fileRequirements')}</div>
              <ul className="list-disc pl-4 text-[12px] text-white/50 space-y-1.5 mb-5 marker:text-white/30">
                <li>{t('customize.mdRequirement')}</li>
                <li>{t('customize.zipRequirement')}</li>
              </ul>
              <div className="text-[12px] text-white/50">
                <a href="#" className="underline hover:text-white/70 underline-offset-2 transition-colors">{t('customize.readMore')}</a>
                <span> {t('customize.or')} </span>
                <a href="#" className="underline hover:text-white/70 underline-offset-2 transition-colors">{t('customize.seeExample')}</a>
              </div>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

export default CustomizePage;
