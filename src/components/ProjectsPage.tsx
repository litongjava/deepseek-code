import React, { useState, useEffect, useCallback, useRef, useMemo } from 'react';
import { Search, Plus, ChevronDown, MoreVertical, Star, ArrowUp, FileText, Trash, Pencil, MessageSquare, X, Upload, Check, AudioLines, ChevronRight, Archive } from 'lucide-react';
import { useNavigate, useSearchParams } from 'react-router-dom';
import { Paperclip, ListCollapse } from 'lucide-react';
import { getProjects, getProject, updateProject, deleteProject, uploadProjectFile, deleteProjectFile, createProjectConversation, deleteConversation, getSkills, Project, ProjectFile } from '../api';
import ModelSelector from './ModelSelector';
import { useModelCatalog } from '../features/model-catalog/useModelCatalog';
import { useChatStore } from '../stores/useChatStore';
import { IconPlus } from './Icons';
import CreateProjectDialog from '../features/project/CreateProjectDialog';
import ProjectWorkspaceBar from '../features/project/ProjectWorkspaceBar';
import TrustProjectDialog from '../features/project/TrustProjectDialog';
import startProjectsImg from '../assets/icons/start-projects.png';
import { useI18n } from '../hooks/useI18n';

const ProjectsPage = () => {
  const { t } = useI18n();
  const navigate = useNavigate();
  const [searchParams, setSearchParams] = useSearchParams();
  const [searchQuery, setSearchQuery] = useState('');
  const [isCreating, setIsCreating] = useState(false);
  const [projects, setProjects] = useState<Project[]>([]);
  const [loading, setLoading] = useState(true);
  const [currentProject, setCurrentProject] = useState<any>(null);
  const [projectToTrust, setProjectToTrust] = useState<Project | null>(null);
  const [projectError, setProjectError] = useState('');
  const [editingInstructions, setEditingInstructions] = useState(false);
  const [instructionsText, setInstructionsText] = useState('');
  const [uploading, setUploading] = useState(false);
  const [showMenu, setShowMenu] = useState(false);
  const [editingName, setEditingName] = useState(false);
  const [editName, setEditName] = useState('');
  const [sortMenuOpen, setSortMenuOpen] = useState(false);
  const [sortBy, setSortBy] = useState<'activity' | 'edited' | 'created'>('activity');
  const [activeMenu, setActiveMenu] = useState<string | null>(null);
  const [projectToDelete, setProjectToDelete] = useState<Project | null>(null);
  const [projectToEdit, setProjectToEdit] = useState<Project | null>(null);
  const [editDetailsName, setEditDetailsName] = useState('');
  const [editDetailsDesc, setEditDetailsDesc] = useState('');
  const [message, setMessage] = useState('');
  const fileInputRef = useRef<HTMLInputElement>(null);
  const textareaRef = useRef<HTMLTextAreaElement>(null);
  const [showPlusMenu, setShowPlusMenu] = useState(false);
  const [showSkillsSubmenu, setShowSkillsSubmenu] = useState(false);
  const [enabledSkills, setEnabledSkills] = useState<Array<{ id: string; name: string; description?: string }>>([]);
  const [selectedSkill, setSelectedSkill] = useState<{ name: string; slug: string; description?: string } | null>(null);
  const plusMenuRef = useRef<HTMLDivElement>(null);
  const plusBtnRef = useRef<HTMLButtonElement>(null);

  const viewingIdRef = useRef<string | null>(null);
  const { selectorModels, isModelSelectable, resolveModelForNewChat } = useModelCatalog(viewingIdRef);
  const currentModelString = useChatStore(s => s.currentModel);
  const setCurrentModelString = useChatStore(s => s.setCurrentModel);
  const reasoningMode = useChatStore(s => s.reasoningMode);
  const setReasoningMode = useChatStore(s => s.setReasoningMode);
  useEffect(() => {
    if (selectorModels.length && !isModelSelectable(currentModelString)) {
      setCurrentModelString(resolveModelForNewChat(currentModelString));
    }
  }, [currentModelString, selectorModels, isModelSelectable, resolveModelForNewChat, setCurrentModelString]);
  const handleModelChange = (newModelString: string) => {
    if (isModelSelectable(newModelString)) setCurrentModelString(newModelString);
  };

  const ensureProjectReady = async () => {
    const project: Project = await getProject(currentProject.id);
    if (!project.folders?.length) {
      setProjectError('Add at least one source folder before starting a chat.');
      return false;
    }
    if (project.folders.some(folder => !folder.trusted)) { setProjectToTrust(project); return false; }
    setProjectError('');
    return true;
  };

  const handleChatSubmit = async () => {
    if (!message.trim() || !currentProject || !isModelSelectable(currentModelString)) return;
    try {
      if (!await ensureProjectReady()) return;
      const conv = await createProjectConversation(currentProject.id, message.slice(0, 50), currentModelString);
      navigate(`/chat/${conv.id}`, { state: { initialMessage: message, model: currentModelString } });
      setMessage('');
    } catch (err) {
      console.error(err);
    }
  };

  const loadProjects = useCallback(async () => {
    try {
      const data = await getProjects();
      setProjects(Array.isArray(data) ? data : []);
    } catch (err) {
      console.error('Failed to load projects:', err);
      setProjects([]);
    }
    setLoading(false);
  }, []);

  useEffect(() => { loadProjects(); }, [loadProjects]);

  useEffect(() => {
    if (!showPlusMenu) { setShowSkillsSubmenu(false); return; }
    getSkills().then((data: any) => {
      const all = [...(data.examples || []), ...(data.my_skills || [])];
      setEnabledSkills(all.filter((s: any) => s.enabled).map((s: any) => ({ id: s.id, name: s.name, description: s.description })));
    }).catch(() => {});
  }, [showPlusMenu]);

  useEffect(() => {
    if (!showPlusMenu) return;
    const handleClick = (e: MouseEvent) => {
      if (plusMenuRef.current && !plusMenuRef.current.contains(e.target as Node) &&
        plusBtnRef.current && !plusBtnRef.current.contains(e.target as Node)) {
        setShowPlusMenu(false);
      }
    };
    document.addEventListener('mousedown', handleClick);
    return () => document.removeEventListener('mousedown', handleClick);
  }, [showPlusMenu]);

  const loadProject = useCallback(async (id: string) => {
    try {
      const data = await getProject(id);
      setCurrentProject(data);
      setInstructionsText(data.instructions || '');
    } catch (_) { }
  }, []);

  useEffect(() => {
    const id = searchParams.get('project');
    if (id) loadProject(id);
    else setCurrentProject(null);
  }, [searchParams, loadProject]);

  useEffect(() => {
    const refresh = () => { loadProjects(); if (currentProject?.id) loadProject(currentProject.id); };
    window.addEventListener('projectsChanged', refresh);
    return () => window.removeEventListener('projectsChanged', refresh);
  }, [loadProjects, loadProject, currentProject?.id]);

  const handleDelete = async () => {
    if (!currentProject) return;
    if (!window.confirm(t('customize.confirmDeleteProjectMsg', { name: currentProject.name }))) return;
    try {
      await deleteProject(currentProject.id);
      window.dispatchEvent(new Event('projectsChanged'));
      setSearchParams({});
      setCurrentProject(null);
      setShowMenu(false);
      loadProjects();
    } catch (_) { }
  };

  const handleDeleteProject = async (p: Project) => {
    try {
      await deleteProject(p.id);
      window.dispatchEvent(new Event('projectsChanged'));
      if (searchParams.get('project') === p.id) setSearchParams({});
      if (currentProject && currentProject.id === p.id) {
        setCurrentProject(null);
      }
      setProjectToDelete(null);
      loadProjects();
    } catch (_) { }
  };

  const handleSaveEditDetails = async () => {
    if (!projectToEdit) return;
    try {
      await updateProject(projectToEdit.id, {
        name: editDetailsName,
        description: editDetailsDesc
      });
      window.dispatchEvent(new Event('projectsChanged'));
      setProjectToEdit(null);
      loadProjects();
      if (currentProject && currentProject.id === projectToEdit.id) {
        loadProject(currentProject.id);
      }
    } catch (_) { }
  };

  const handleSaveInstructions = async () => {
    if (!currentProject) return;
    await updateProject(currentProject.id, { instructions: instructionsText });
    setEditingInstructions(false);
    loadProject(currentProject.id);
  };

  const handleFileUpload = async (files: FileList | File[]) => {
    if (!currentProject) return;
    setUploading(true);
    for (const file of Array.from(files)) {
      try {
        await uploadProjectFile(currentProject.id, file);
      } catch (_) { }
    }
    setUploading(false);
    loadProject(currentProject.id);
  };

  const handleDeleteFile = async (fileId: string) => {
    if (!currentProject) return;
    await deleteProjectFile(currentProject.id, fileId);
    loadProject(currentProject.id);
  };

  const handleNewChat = async () => {
    if (!currentProject) return;
    try {
      if (!await ensureProjectReady()) return;
      const conv = await createProjectConversation(currentProject.id);
      navigate(`/chat/${conv.id}`);
    } catch (_) { }
  };

  const handleDeleteConversation = async (convId: string, e: React.MouseEvent) => {
    e.stopPropagation();
    if (!currentProject) return;
    try {
      await deleteConversation(convId);
      loadProject(currentProject.id);
      loadProjects();
    } catch (_) { }
  };

  const handleRenameSave = async () => {
    if (!currentProject || !editName.trim()) return;
    await updateProject(currentProject.id, { name: editName.trim() });
    window.dispatchEvent(new Event('projectsChanged'));
    setEditingName(false);
    loadProject(currentProject.id);
    loadProjects();
  };

  const filteredProjects = useMemo(() => {
    const filtered = projects.filter(p =>
      (p.name || '').toLowerCase().includes(searchQuery.toLowerCase()) ||
      (p.description || '').toLowerCase().includes(searchQuery.toLowerCase())
    );
    return [...filtered].sort((a, b) => {
      if (sortBy === 'created') return new Date(b.created_at).getTime() - new Date(a.created_at).getTime();
      return new Date(b.updated_at).getTime() - new Date(a.updated_at).getTime();
    });
  }, [projects, searchQuery, sortBy]);

  if (currentProject) {
    return (
      <div className="flex-1 h-full bg-deepseek-bg overflow-y-auto">
        <div className="max-w-[800px] mx-auto px-8 py-12">
          {projectError && <p role="alert" className="my-3 text-sm text-red-500">{projectError}</p>}
          {projectToTrust && <TrustProjectDialog project={projectToTrust} onClose={() => setProjectToTrust(null)} onTrusted={project => { setProjectToTrust(null); loadProject(project.id); }} />}
          <div className="flex items-start justify-between mb-4 gap-4">
            <div className="flex-1 min-w-0">
              {editingName ? (
                <div className="flex items-center gap-2">
                  <input
                    autoFocus
                    value={editName}
                    onChange={e => setEditName(e.target.value)}
                    onKeyDown={e => { if (e.key === 'Enter') handleRenameSave(); if (e.key === 'Escape') setEditingName(false); }}
                    className="font-[Spectral] text-[32px] text-deepseek-text bg-transparent border-b-2 border-deepseek-accent outline-none w-full"
                    style={{ fontWeight: 500 }}
                  />
                </div>
              ) : (
                <h1
                  className="font-[Spectral] text-[32px] text-deepseek-text leading-tight mb-2"
                  style={{ fontWeight: 500 }}
                >
                  {currentProject.name}
                </h1>
              )}
              {currentProject.description && (
                <p className="text-[15.5px] text-deepseek-textSecondary">{currentProject.description}</p>
              )}
            </div>
            <div className="flex items-center gap-1 text-deepseek-textSecondary mt-2 flex-shrink-0">
              <button className="p-1 hover:text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 rounded-md transition-colors"><MoreVertical size={18} /></button>
            </div>
          </div>

          <div className="mb-4">
            <ProjectWorkspaceBar projectId={currentProject.id} menuPlacement="below" onSelect={project => { setSearchParams({ project: project.id }); loadProject(project.id); }} />
          </div>

          <div className="space-y-4">
            <div
              className="bg-deepseek-input border border-deepseek-border dark:border-[#3a3a38] shadow-[0_2px_8px_rgba(0,0,0,0.02)] hover:shadow-[0_2px_8px_rgba(0,0,0,0.08)] hover:border-[#CCC] dark:hover:border-[#5a5a58] focus-within:shadow-[0_2px_8px_rgba(0,0,0,0.08)] focus-within:border-[#CCC] dark:focus-within:border-[#5a5a58] transition-all duration-200 flex flex-col max-h-[60vh] font-sans rounded-2xl"
            >
              <div className="flex-1 overflow-y-auto min-h-0">
                <div className="relative">
                  {message.match(/^\/[a-zA-Z0-9_-]+/) && (
                    <div className="pl-5 pr-4 pt-5 pb-1 text-[16px] font-sans font-[350]" style={{ minHeight: '48px', position: 'absolute', top: 0, left: 0, right: 0, pointerEvents: 'none', whiteSpace: 'pre-wrap', wordBreak: 'break-word' }} aria-hidden>
                      {(() => { const m = message.match(/^(\/[a-zA-Z0-9_-]+)([\s\S]*)$/); return m ? <><span className="text-[#4B9EFA]">{m[1]}</span><span className="text-deepseek-text">{m[2]}</span></> : null; })()}
                    </div>
                  )}
                  <textarea
                    ref={textareaRef}
                    className={`w-full pl-5 pr-4 pt-5 pb-1 placeholder:text-deepseek-textSecondary text-[16px] outline-none resize-none overflow-hidden bg-transparent font-sans font-[350] ${message.match(/^\/[a-zA-Z0-9_-]+/) ? 'text-transparent caret-deepseek-text' : 'text-deepseek-text'}`}
                    style={{ minHeight: '48px', borderRadius: '16px 16px 0 0' }}
                    placeholder={selectedSkill ? t('customize.describeSkillAction', { name: selectedSkill.name }) : t('customize.howCanIHelp')}
                    value={message}
                    onChange={(e) => {
                      setMessage(e.target.value);
                      e.target.style.height = 'auto';
                      e.target.style.height = Math.min(e.target.scrollHeight, 300) + 'px';
                      e.target.style.overflowY = e.target.scrollHeight > 300 ? 'auto' : 'hidden';
                    }}
                    onKeyDown={e => {
                      if (e.key === 'Backspace' && selectedSkill) {
                        const pos = (e.target as HTMLTextAreaElement).selectionStart;
                        const prefix = `/${selectedSkill.slug} `;
                        if (pos > 0 && pos <= prefix.length && message.startsWith(prefix.slice(0, pos))) {
                          e.preventDefault();
                          setMessage(message.slice(prefix.length));
                          setSelectedSkill(null);
                          return;
                        }
                      }
                      if (e.key === 'Enter' && !e.shiftKey) {
                        e.preventDefault();
                        handleChatSubmit();
                      }
                    }}
                  />
                </div>
              </div>
              <div className="px-4 pb-3 pt-1 flex items-center justify-between flex-shrink-0">
                <div className="relative flex items-center">
                  <button
                    ref={plusBtnRef}
                    onClick={() => setShowPlusMenu(prev => !prev)}
                    className="p-2 text-deepseek-textSecondary hover:text-deepseek-text hover:bg-deepseek-hover rounded-lg transition-colors"
                  >
                    <IconPlus size={20} />
                  </button>
                  {showPlusMenu && (
                    <div ref={plusMenuRef} className="absolute bottom-full left-0 mb-2 w-[220px] bg-deepseek-input border border-deepseek-border rounded-xl shadow-[0_4px_16px_rgba(0,0,0,0.12)] py-1.5 z-50">
                      <button onClick={() => { setShowPlusMenu(false); fileInputRef.current?.click(); }} className="w-full flex items-center gap-3 px-4 py-2.5 text-[13px] text-deepseek-text hover:bg-deepseek-hover transition-colors">
                        <Paperclip size={16} className="text-deepseek-textSecondary" />
                        {t('customize.addFiles')}
                      </button>
                      <div className="relative">
                        <button onMouseEnter={() => setShowSkillsSubmenu(true)} onClick={() => setShowSkillsSubmenu(p => !p)} className="w-full flex items-center justify-between px-4 py-2.5 text-[13px] text-deepseek-text hover:bg-deepseek-hover transition-colors">
                          <div className="flex items-center gap-3"><FileText size={16} className="text-deepseek-textSecondary" />{t('customize.skills')}</div>
                          <ChevronDown size={14} className="text-deepseek-textSecondary -rotate-90" />
                        </button>
                        {showSkillsSubmenu && (
                          <div className="absolute left-full bottom-0 ml-1 w-[200px] bg-deepseek-input border border-deepseek-border rounded-xl shadow-[0_4px_16px_rgba(0,0,0,0.12)] py-1.5 z-50 max-h-[300px] overflow-y-auto" onMouseLeave={() => setShowSkillsSubmenu(false)}>
                            {enabledSkills.length > 0 ? enabledSkills.map(skill => (
                              <button key={skill.id} onClick={() => {
                                setShowPlusMenu(false); setShowSkillsSubmenu(false);
                                const slug = skill.name.toLowerCase().replace(/\s+/g, '-');
                                setSelectedSkill({ name: skill.name, slug, description: skill.description });
                                setMessage(prev => prev ? `/${slug} ${prev}` : `/${slug} `);
                                textareaRef.current?.focus();
                              }} className="w-full text-left px-4 py-2 text-[13px] text-deepseek-text hover:bg-deepseek-hover transition-colors truncate">{skill.name}</button>
                            )) : <div className="px-4 py-2 text-[12px] text-deepseek-textSecondary italic">{t('customize.noSkillsEnabled')}</div>}
                            <div className="border-t border-deepseek-border mt-1 pt-1">
                              <button onClick={() => { setShowPlusMenu(false); window.location.hash = '#/customize'; }} className="w-full flex items-center gap-3 px-4 py-2 text-[13px] text-deepseek-textSecondary hover:bg-deepseek-hover transition-colors"><FileText size={14} />{t('customize.manageSkills')}</button>
                            </div>
                          </div>
                        )}
                      </div>
                    </div>
                  )}
                </div>
                <div className="flex items-center gap-3">
                  <ModelSelector
                    currentModelString={currentModelString}
                    models={selectorModels}
                    onModelChange={handleModelChange}
                    effort={reasoningMode}
                    onEffortChange={setReasoningMode}
                    isNewChat={true}
                  />
                  <button
                    onClick={handleChatSubmit}
                    aria-label="Send message"
                    disabled={!message.trim() || !isModelSelectable(currentModelString)}
                    className="p-2 bg-[#C6613F] text-white rounded-lg hover:bg-[#D97757] transition-colors disabled:opacity-40 disabled:cursor-not-allowed"
                  >
                    <ArrowUp size={22} strokeWidth={2.5} />
                  </button>
                </div>
              </div>
            </div>

            {currentProject.conversations && currentProject.conversations.length > 0 ? (
              <div className="border border-deepseek-border rounded-[16px] overflow-hidden bg-transparent mt-2">
                <div className="px-5 py-3 text-[13px] font-medium text-deepseek-textSecondary border-b border-deepseek-border">
                  {currentProject.conversations.length} {currentProject.conversations.length > 1 ? t('sidebar.chats') : t('sidebar.chats')}
                </div>
                {currentProject.conversations.map((conv: any) => (
                  <div
                    key={conv.id}
                    onClick={() => navigate(`/chat/${conv.id}`)}
                    className="px-5 py-3 flex items-center gap-3 hover:bg-deepseek-hover cursor-pointer border-b border-deepseek-border last:border-b-0 transition-colors group"
                  >
                    <MessageSquare size={16} className="text-deepseek-textSecondary flex-shrink-0" />
                    <span className="text-[14px] text-deepseek-text truncate">{conv.title}</span>
                    <span className="text-[12px] text-deepseek-textSecondary ml-auto flex-shrink-0">
                      {new Date(conv.created_at).toLocaleDateString()}
                    </span>
                    <button
                      onClick={(e) => handleDeleteConversation(conv.id, e)}
                      className="p-1 text-deepseek-textSecondary hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity flex-shrink-0"
                      title={t('common.delete')}
                    >
                      <Trash size={14} />
                    </button>
                  </div>
                ))}
              </div>
            ) : (
              <div className="w-full border border-deepseek-border rounded-[16px] px-6 py-10 flex items-center justify-center bg-transparent mt-2">
                <span className="text-[14.5px] text-[#A1A1AA]">
                  {t('customize.startChatDesc')}
                </span>
              </div>
            )}

            <div className="w-full border border-deepseek-border rounded-[16px] overflow-hidden bg-transparent mt-2">
              <div
                className="p-5 border-b border-deepseek-border hover:bg-black/[0.015] dark:hover:bg-white/[0.015] transition-colors cursor-pointer group"
                onClick={() => { if (!editingInstructions) setEditingInstructions(true); }}
              >
                <div className="flex items-center justify-between">
                  <div className="flex-1">
                    <h3 className="font-semibold text-deepseek-text mb-0.5" style={{ fontSize: '15.5px' }}>{t('customize.instructions')}</h3>
                    {!editingInstructions && (
                      <p className="text-[13px] text-[#A1A1AA]">
                        {currentProject.instructions
                          ? currentProject.instructions.slice(0, 200) + (currentProject.instructions.length > 200 ? '...' : '')
                          : t('customize.addInstructions')}
                      </p>
                    )}
                  </div>
                  {!editingInstructions && (
                    <button className="text-[#A1A1AA] hover:text-deepseek-text transition-colors">
                      {currentProject.instructions ? <Pencil size={18} strokeWidth={1.5} /> : <Plus size={22} strokeWidth={1.5} />}
                    </button>
                  )}
                </div>
                {editingInstructions && (
                  <div
                    className="fixed inset-0 z-50 flex items-center justify-center bg-black/70"
                    onClick={() => { setEditingInstructions(false); setInstructionsText(currentProject.instructions || ''); }}
                  >
                    <div
                      className="w-full max-w-[800px] bg-white dark:bg-[#2A2928] border border-deepseek-border rounded-[20px] shadow-2xl p-7"
                      onClick={e => e.stopPropagation()}
                    >
                      <h2 className="text-[20px] font-bold text-deepseek-text mb-2">{t('customize.setInstructionsTitle')}</h2>
                      <p className="text-[14px] text-[#A1A1AA] mb-5" dangerouslySetInnerHTML={{ __html: t('customize.setInstructionsDesc', { name: currentProject.name }) }} />

                      <textarea
                        autoFocus
                        value={instructionsText}
                        onChange={e => setInstructionsText(e.target.value)}
                        placeholder={t('customize.instructionsPlaceholder')}
                        className="w-full h-[400px] px-4 py-3 bg-deepseek-bg dark:bg-[#202020] border border-deepseek-border rounded-[12px] text-[15px] text-deepseek-text resize-none outline-none focus:border-[#3A7ADA] focus:ring-1 focus:ring-[#3A7ADA] transition-colors"
                      />

                      <div className="flex justify-end gap-3 mt-5">
                        <button
                          onClick={() => { setEditingInstructions(false); setInstructionsText(currentProject.instructions || ''); }}
                          className="px-4 py-2 text-[14px] font-medium text-deepseek-text hover:bg-white/5 border border-transparent hover:border-deepseek-border rounded-xl transition-all"
                        >
                          {t('customize.cancelBtn')}
                        </button>
                        <button
                          onClick={handleSaveInstructions}
                          className="px-4 py-2 text-[14px] font-medium bg-[#E6E6E6] text-[#222] rounded-xl hover:opacity-90 transition-opacity"
                        >
                          {t('customize.saveInstructionsBtn')}
                        </button>
                      </div>
                    </div>
                  </div>
                )}
              </div>

              <div className="p-5 pb-6">
                <div className="flex items-center justify-between mb-4">
                  <h3 className="font-semibold text-deepseek-text" style={{ fontSize: '15.5px' }}>
                    {t('customize.files')} {currentProject.files?.length > 0 && <span className="text-deepseek-textSecondary text-[13px] ml-1">({currentProject.files.length})</span>}
                  </h3>
                  <button
                    onClick={() => fileInputRef.current?.click()}
                    className="text-[#A1A1AA] hover:text-deepseek-text transition-colors"
                  >
                    <Plus size={22} strokeWidth={1.5} />
                  </button>
                  <input
                    ref={fileInputRef}
                    type="file"
                    multiple
                    className="hidden"
                    onChange={e => { if (e.target.files) handleFileUpload(e.target.files); e.target.value = ''; }}
                  />
                </div>

                {uploading && (
                  <div className="text-[13px] text-deepseek-textSecondary animate-pulse mb-3">{t('customize.uploading')}</div>
                )}

                {currentProject.files && currentProject.files.length > 0 ? (
                  <div className="space-y-2">
                    {currentProject.files.map((f: ProjectFile) => (
                      <div key={f.id} className="flex items-center gap-3 px-3 py-2.5 rounded-[12px] bg-black/[0.02] dark:bg-white/[0.03] group border border-transparent hover:border-deepseek-border transition-all">
                        <FileText size={16} className="text-[#A1A1AA] flex-shrink-0" />
                        <div className="flex-1 min-w-0">
                          <div className="text-[13.5px] text-deepseek-text truncate font-medium">{f.file_name}</div>
                          <div className="text-[11.5px] text-[#A1A1AA]">
                            {f.file_size > 1024 * 1024 ? `${(f.file_size / 1024 / 1024).toFixed(1)} MB` : `${(f.file_size / 1024).toFixed(1)} KB`}
                          </div>
                        </div>
                        <button
                          onClick={() => handleDeleteFile(f.id)}
                          className="p-1 text-[#A1A1AA] hover:text-red-500 opacity-0 group-hover:opacity-100 transition-opacity"
                        >
                          <X size={16} />
                        </button>
                      </div>
                    ))}
                  </div>
                ) : (
                  <div
                    className="w-full bg-[#FAFAFA] dark:bg-[#191919] rounded-[16px] flex flex-col items-center justify-center py-8 border border-transparent dark:border-white/[0.04] cursor-pointer hover:bg-[#F3F3F3] dark:hover:bg-[#222222] transition-colors"
                    onClick={() => fileInputRef.current?.click()}
                    onDragOver={e => { e.preventDefault(); e.stopPropagation(); }}
                    onDrop={e => { e.preventDefault(); e.stopPropagation(); if (e.dataTransfer.files.length) handleFileUpload(e.dataTransfer.files); }}
                  >
                    <div className="flex items-center justify-center mb-3">
                      <div className="w-[84px] h-[48px] relative opacity-60 mix-blend-luminosity grayscale">
                        <div className="absolute right-[4px] bottom-0 w-[28px] h-[36px] bg-[#3B3B3B] border border-[#555] rounded-[4px] flex flex-col items-center py-1.5 px-1 gap-[3px] shadow-sm transform translate-x-2 translate-y-2 -rotate-12 z-0">
                          <div className="w-full h-[1.5px] bg-[#666] rounded-full mx-1"></div>
                          <div className="w-3/4 h-[1.5px] bg-[#666] rounded-full mx-1 self-start"></div>
                        </div>
                        <div className="absolute left-[4px] bottom-0 w-[28px] h-[36px] bg-[#3B3B3B] border border-[#555] rounded-[4px] flex flex-col items-center py-1.5 px-1 gap-[3px] shadow-sm transform -translate-x-2 translate-y-1 rotate-12 z-0">
                          <div className="w-full h-[1.5px] bg-[#666] rounded-full mx-1"></div>
                          <div className="w-full h-[1.5px] bg-[#666] rounded-full mx-1"></div>
                          <div className="w-1/2 h-[1.5px] bg-[#666] rounded-full mx-1 self-start"></div>
                        </div>
                        <div className="absolute left-1/2 bottom-0 -translate-x-1/2 w-[34px] h-[42px] bg-[#444] border border-[#666] rounded-[6px] shadow-md flex flex-col items-center py-2 px-1.5 gap-[4px] z-10">
                          <div className="w-[12px] h-[12px] bg-[#555] rounded-sm flex items-center justify-center self-end mb-0.5"><Plus size={8} className="text-white" /></div>
                          <div className="w-full h-[2px] bg-[#888] rounded-full mx-1"></div>
                          <div className="w-full h-[2px] bg-[#888] rounded-full mx-1"></div>
                          <div className="w-2/3 h-[2px] bg-[#888] rounded-full mx-1 self-start"></div>
                        </div>
                      </div>
                    </div>
                    <span className="text-[13px] text-[#A1A1AA] text-center max-w-[200px] leading-relaxed">
                      {t('customize.addFilesDesc')}
                    </span>
                  </div>
                )}
              </div>
            </div>
          </div>
        </div>
      </div>
    );
  }

  if (isCreating) {
    return <CreateProjectDialog onClose={() => setIsCreating(false)} onCreated={project => {
      setIsCreating(false);
      navigate('/', { state: { createdProjectId: project.id } });
    }} />;
  }

  return (
    <div className="flex-1 h-full bg-deepseek-bg overflow-y-auto">
      <div className="max-w-[800px] mx-auto px-8 py-12">
        <div className="flex items-center justify-between mb-8">
          <h1 className="font-[Spectral] text-[32px] text-deepseek-text" style={{ fontWeight: 500 }}>{t('projects.title')}</h1>
          <button
            onClick={() => setIsCreating(true)}
            className="flex items-center gap-2 px-3.5 py-1.5 bg-deepseek-text text-deepseek-bg hover:opacity-90 rounded-lg transition-opacity font-medium"
            style={{ fontSize: '14px' }}
          >
            <Plus size={16} strokeWidth={2.5} />
            {t('customize.newProject')}
          </button>
        </div>

        {projects.length > 0 && (
          <>
            <div className="relative mb-6">
              <div className="absolute inset-y-0 left-3 flex items-center pointer-events-none">
                <Search className="h-5 w-5 text-deepseek-textSecondary opacity-80" />
              </div>
              <input
                type="text"
                placeholder={t('customize.searchProjects')}
                value={searchQuery}
                onChange={e => setSearchQuery(e.target.value)}
                className="w-full pl-10 pr-4 py-3 bg-white dark:bg-deepseek-input border border-gray-200 dark:border-deepseek-border rounded-xl text-deepseek-text placeholder-deepseek-textSecondary focus:outline-none focus:ring-2 focus:ring-blue-500/20 focus:border-blue-500 transition-all text-[15px]"
              />
            </div>

            <div className="flex justify-end mb-6">
              <div className="flex items-center gap-3 text-[14.5px] text-[#A1A1AA] relative">
                <span>{t('customize.sortBy')}</span>
                <button
                  onClick={() => setSortMenuOpen(!sortMenuOpen)}
                  className={`flex items-center gap-2 text-deepseek-text border border-[#3A3A3A] hover:border-[#4A4A4A] dark:border-deepseek-border dark:hover:bg-deepseek-hover rounded-[10px] px-3.5 py-1.5 transition-colors ${sortMenuOpen ? 'bg-deepseek-hover' : ''}`}
                >
                  {sortBy === 'activity' ? t('customize.recentActivity') : sortBy === 'edited' ? t('customize.lastEdited') : t('customize.dateCreated')}
                  <ChevronDown size={14} className="text-deepseek-textSecondary" />
                </button>
                {sortMenuOpen && (
                  <>
                    <div className="fixed inset-0 z-40" onClick={() => setSortMenuOpen(false)} />
                    <div className="absolute top-full right-0 mt-1.5 w-[200px] bg-white dark:bg-[#2A2928] border border-gray-200 dark:border-deepseek-border rounded-[14px] shadow-lg py-1.5 z-50">
                      {[
                        { id: 'activity', label: t('customize.recentActivity') },
                        { id: 'edited', label: t('customize.lastEdited') },
                        { id: 'created', label: t('customize.dateCreated') },
                      ].map(opt => (
                        <button
                          key={opt.id}
                          onClick={() => {
                            setSortBy(opt.id as any);
                            setSortMenuOpen(false);
                          }}
                          className="w-full flex items-center justify-between px-4 py-2.5 text-[15px] text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors"
                        >
                          {opt.label}
                          {sortBy === opt.id && <Check size={16} className="text-deepseek-text opacity-80" />}
                        </button>
                      ))}
                    </div>
                  </>
                )}
              </div>
            </div>
          </>
        )}

        {loading ? (
          <div className="text-center text-deepseek-textSecondary text-[14px] mt-12">{t('customize.loadingProjects')}</div>
        ) : filteredProjects.length > 0 ? (
          <div className="grid grid-cols-1 md:grid-cols-2 gap-4">
            {filteredProjects.map(p => (
              <div
                key={p.id}
                onClick={() => setSearchParams({ project: p.id })}
                className="flex flex-col p-5 border border-deepseek-border rounded-[12px] bg-transparent hover:bg-black/[0.02] dark:hover:bg-white/[0.02] cursor-pointer transition-colors group min-h-[170px]"
              >
                <div className="flex items-center justify-between mb-2.5 relative">
                  <div className="flex items-center gap-3">
                    <h3 className="text-[15.5px] font-medium text-deepseek-text truncate">{p.name}</h3>
                  </div>
                  <div className="relative" onClick={(e) => e.stopPropagation()}>
                    <button
                      onClick={(e) => { e.stopPropagation(); setActiveMenu(activeMenu === p.id ? null : p.id); }}
                      className={`p-1 text-[#A1A1AA] hover:text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 rounded-[6px] transition-all ${activeMenu === p.id ? 'opacity-100 bg-black/5 dark:bg-white/5' : 'opacity-0 group-hover:opacity-100'}`}
                    >
                      <MoreVertical size={18} />
                    </button>

                    {activeMenu === p.id && (
                      <>
                        <div className="fixed inset-0 z-40" onClick={(e) => { e.stopPropagation(); setActiveMenu(null); }} />
                        <div className="absolute top-full right-0 mt-1 w-[180px] bg-white dark:bg-[#30302E] rounded-[16px] shadow-[0_4px_24px_rgba(0,0,0,0.15)] border border-gray-200 dark:border-[#65645F] py-1.5 z-50">
                          <button className="w-full flex items-center gap-3 px-4 py-2.5 text-[14px] text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors text-left" onClick={(e) => { e.stopPropagation(); setActiveMenu(null); }}>
                            <Star size={16} className="text-deepseek-textSecondary" />
                            {t('customize.star')}
                          </button>
                          <button className="w-full flex items-center gap-3 px-4 py-2.5 text-[14px] text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors text-left" onClick={(e) => { e.stopPropagation(); setActiveMenu(null); setProjectToEdit(p); setEditDetailsName(p.name); setEditDetailsDesc(p.description || ''); }}>
                            <Pencil size={16} className="text-deepseek-textSecondary" />
                            {t('customize.editDetails')}
                          </button>
                          <div className="my-1.5 border-t border-deepseek-border opacity-50" />
                          <button className="w-full flex items-center gap-3 px-4 py-2.5 text-[14px] text-deepseek-text hover:bg-black/5 dark:hover:bg-white/5 transition-colors text-left" onClick={(e) => { e.stopPropagation(); setActiveMenu(null); }}>
                            <Archive size={16} className="text-deepseek-textSecondary" />
                            {t('customize.archive')}
                          </button>
                          <button className="w-full flex items-center gap-3 px-4 py-2.5 text-[14px] text-[#E05A5A] hover:bg-red-500/10 transition-colors text-left" onClick={(e) => { e.stopPropagation(); setActiveMenu(null); setProjectToDelete(p); }}>
                            <Trash size={16} className="text-[#E05A5A]" />
                            {t('common.delete')}
                          </button>
                        </div>
                      </>
                    )}
                  </div>
                </div>

                <p className="text-[14px] text-deepseek-textSecondary line-clamp-3 leading-relaxed flex-1">
                  {p.description || t('customize.noDescriptionProvided')}
                </p>

                <div className="mt-4 pt-1 flex items-center gap-4 text-[12px] text-deepseek-textSecondary/80">
                  <span>{t('customize.updated')} {new Date(p.updated_at).toLocaleDateString()}</span>
                  {(p.file_count ?? 0) > 0 && <span>• {p.file_count} {t('customize.files')}</span>}
                  {(p.chat_count ?? 0) > 0 && <span>• {p.chat_count} {t('sidebar.chats')}</span>}
                </div>
              </div>
            ))}
          </div>
        ) : (
          <div className="flex flex-col items-center justify-center mt-12">
            <img src={startProjectsImg} alt="Start a project" className="w-[100px] h-auto mb-6 dark:invert opacity-90" />
            <h2 className="text-[17px] font-medium text-deepseek-text mb-3">{t('customize.lookingForProject')}</h2>
            <p className="text-[15px] text-deepseek-textSecondary text-center max-w-[400px] leading-relaxed mb-6">
              {t('customize.lookingForProjectDesc')}
            </p>
            <button
              onClick={() => setIsCreating(true)}
              className="flex items-center gap-2 px-4 py-2 bg-transparent border border-deepseek-border hover:bg-deepseek-hover rounded-xl text-deepseek-text transition-colors text-[14.5px] font-medium"
            >
              <Plus size={18} strokeWidth={2.5} />
              {t('customize.newProject')}
            </button>
          </div>
        )}
      </div>

      {projectToDelete && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50 backdrop-blur-sm p-4">
          <div className="bg-deepseek-input w-[460px] rounded-[16px] flex flex-col shadow-2xl relative border border-deepseek-border overflow-hidden">
            <div className="px-6 pt-6 pb-4 text-left">
              <h3 className="text-[19px] font-semibold text-deepseek-text mb-3">{t('customize.deleteProjectTitle')}</h3>
              <p className="text-[15px] text-deepseek-textSecondary leading-relaxed pr-4">
                {t('customize.confirmDeleteProjectMsg', { name: projectToDelete.name })}
              </p>
            </div>
            <div className="px-5 pb-5 pt-2 flex justify-end gap-3 mt-4">
              <button
                onClick={() => setProjectToDelete(null)}
                className="px-5 py-2 text-[14.5px] font-medium text-deepseek-text border border-deepseek-border hover:bg-deepseek-hover rounded-[8px] transition-colors"
              >
                {t('common.cancel')}
              </button>
              <button
                onClick={() => handleDeleteProject(projectToDelete)}
                className="px-5 py-2 text-[14.5px] font-medium text-white bg-[#E05A5A] hover:bg-[#E86B6B] rounded-[8px] transition-colors"
              >
                {t('common.delete')}
              </button>
            </div>
          </div>
        </div>
      )}

      {projectToEdit && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50 backdrop-blur-sm p-4">
          <div className="bg-deepseek-input w-[460px] rounded-[16px] flex flex-col shadow-2xl relative border border-deepseek-border overflow-hidden">
            <div className="px-6 pt-6 pb-4 text-left">
              <h3 className="text-[19px] font-semibold text-deepseek-text mb-5">{t('customize.editDetails')}</h3>

              <div className="space-y-4">
                <div>
                  <label className="block text-[14px] text-deepseek-textSecondary mb-2 font-medium">{t('customize.name')}</label>
                  <input
                    type="text"
                    value={editDetailsName}
                    onChange={(e) => setEditDetailsName(e.target.value)}
                    className="w-full px-3 py-2 bg-transparent border border-deepseek-border rounded-[8px] text-deepseek-text outline-none focus:border-[#3A7ADA] focus:ring-1 focus:ring-[#3A7ADA] transition-all text-[15px]"
                    autoFocus
                  />
                </div>
                <div>
                  <label className="block text-[14px] text-deepseek-textSecondary mb-2 font-medium">{t('customize.description')}</label>
                  <textarea
                    value={editDetailsDesc}
                    onChange={(e) => setEditDetailsDesc(e.target.value)}
                    rows={4}
                    className="w-full px-3 py-2 bg-deepseek-bg border border-deepseek-border rounded-[8px] text-deepseek-text outline-none focus:border-[#3A7ADA] focus:ring-1 focus:ring-[#3A7ADA] transition-all resize-none text-[14.5px] leading-relaxed"
                  />
                </div>
              </div>
            </div>

            <div className="px-6 pb-6 pt-2 flex justify-end gap-3 mt-4">
              <button
                onClick={() => setProjectToEdit(null)}
                className="px-5 py-2.5 text-[14.5px] font-medium text-deepseek-text border border-deepseek-border hover:bg-deepseek-hover rounded-[8px] transition-colors"
              >
                {t('common.cancel')}
              </button>
              <button
                onClick={handleSaveEditDetails}
                className="px-5 py-2.5 text-[14.5px] font-medium bg-deepseek-text text-deepseek-bg hover:opacity-90 rounded-[8px] transition-opacity"
              >
                {t('common.save')}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
};

export default ProjectsPage;
