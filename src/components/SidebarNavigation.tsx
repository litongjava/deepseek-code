import { useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { ChevronDown, Folder, Search, Wrench, Puzzle, MessageSquare, Terminal, Cpu, Eye, Code, Palette, Sparkles, Brain, Users, Smartphone, Globe, Network, Github, BookOpen, Monitor } from 'lucide-react';
import { useNavigate } from 'react-router-dom';
import { useI18n } from '../hooks/useI18n';
import { useGeneralSettingsStore } from '../stores/useGeneralSettingsStore';

export default function SidebarNavigation({ collapsed, onNewChat, onSearch, onPanel, onNavigate, onSwarm }: {
  collapsed: boolean; onNewChat: () => void; onSearch: () => void;
  onPanel: (panel: string) => void; onNavigate: (label: string) => void; onSwarm: () => void;
}) {
  const navigate = useNavigate();
  const { t } = useI18n();
  const showCodingView = useGeneralSettingsStore(state => state.showCodingView);
  const [expanded, setExpanded] = useState<string | null>(null);
  const [position, setPosition] = useState({ left: 48, top: 96 });
  const menuRef = useRef<HTMLDivElement>(null);
  const triggerRef = useRef<HTMLButtonElement | null>(null);
  const [toolbar, setToolbar] = useState<HTMLElement | null>(null);
  useEffect(() => { setToolbar(document.getElementById('desktop-tool-menus')); }, []);
  useEffect(() => {
    if (!expanded) return;
    const outside = (event: MouseEvent) => {
      if (!menuRef.current?.contains(event.target as Node) && !triggerRef.current?.contains(event.target as Node)) setExpanded(null);
    };
    const focusOutside = (event: FocusEvent) => {
      if (!menuRef.current?.contains(event.target as Node) && !triggerRef.current?.contains(event.target as Node)) setExpanded(null);
    };
    menuRef.current?.querySelector<HTMLButtonElement>('button')?.focus();
    const key = (event: KeyboardEvent) => {
      if (event.key === 'Escape') { setExpanded(null); triggerRef.current?.focus(); }
    };
    const resize = () => setExpanded(null);
    document.addEventListener('mousedown', outside);
    document.addEventListener('focusin', focusOutside);
    window.addEventListener('keydown', key);
    window.addEventListener('resize', resize);
    return () => { document.removeEventListener('mousedown', outside); document.removeEventListener('focusin', focusOutside); window.removeEventListener('keydown', key); window.removeEventListener('resize', resize); };
  }, [expanded]);
  const tools = [
    { label: t('sidebar.terminal'), icon: Terminal, action: () => onPanel('terminal') },
    ...(showCodingView ? [{ label: t('sidebar.agent'), icon: Cpu, action: () => onPanel('agent') }] : []),
    { label: t('sidebar.preview'), icon: Eye, action: () => onPanel('preview') },
    { label: t('sidebar.code'), icon: Code, action: () => onNavigate('Code') },
    { label: t('sidebar.design'), icon: Palette, action: () => onNavigate('Design') },
    { label: t('sidebar.artifacts'), icon: Sparkles, action: () => onNavigate('Artifacts') },
    { label: t('sidebar.memory'), icon: Brain, action: () => onNavigate('Memory') },
    { label: t('sidebar.knowledge'), icon: BookOpen, action: () => onPanel('knowledge') },
    { label: t('sidebar.appStudio'), icon: Code, action: () => onPanel('appstudio') },
    { label: t('sidebar.swarmCollaboration'), icon: Users, action: onSwarm },
    { label: t('sidebar.computerUse'), icon: Monitor, action: () => onPanel('computerUse') },
    { label: t('sidebar.browser'), icon: Globe, action: () => onPanel('browser') },
  ];
  const extensions = [
    { label: t('sidebar.customize'), icon: Puzzle, action: () => navigate('/customize') },
    { label: t('sidebar.imIntegration'), icon: Network, action: () => onPanel('im') },
    { label: t('sidebar.h5Access'), icon: Smartphone, action: () => onPanel('h5') },
    { label: 'GitHub', icon: Github, action: () => onPanel('github') },
  ];
  const itemClass = 'flex w-full items-center gap-2.5 rounded-lg px-2 py-2 text-left text-[13px] text-deepseek-text hover:bg-deepseek-hover';
  const groups = <div className="flex items-center gap-1" aria-label="Tools and extensions">
    {[{ id: 'tools', label: t('sidebar.tools'), icon: Wrench, items: tools }, { id: 'extensions', label: t('sidebar.extensions'), icon: Puzzle, items: extensions }].map(group => <div key={group.id}>
      <button type="button" aria-label={group.label} aria-haspopup="menu" aria-expanded={expanded === group.id} onClick={event => { triggerRef.current = event.currentTarget; const rect = event.currentTarget.getBoundingClientRect(); setPosition({ left: Math.max(8, Math.min(rect.left, window.innerWidth - 232)), top: rect.bottom + 5 }); setExpanded(expanded === group.id ? null : group.id); }} className="flex h-8 items-center gap-1.5 rounded-md px-2 text-[13px] text-deepseek-text hover:bg-deepseek-hover"><group.icon size={15} className="shrink-0" /><span>{group.label}</span><ChevronDown size={12} /></button>
      {expanded === group.id && createPortal(<div ref={menuRef} role="menu" aria-label={group.label} style={position} onKeyDown={event => {
        if (!['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(event.key)) return;
        event.preventDefault();
        const buttons = Array.from(event.currentTarget.querySelectorAll<HTMLButtonElement>('button'));
        const current = buttons.indexOf(document.activeElement as HTMLButtonElement);
        const index = event.key === 'Home' ? 0 : event.key === 'End' ? buttons.length - 1 : (current + (event.key === 'ArrowDown' ? 1 : buttons.length - 1)) % buttons.length;
        buttons[index]?.focus();
      }} className="fixed z-[10010] max-h-[min(440px,80vh)] w-56 overflow-y-auto rounded-lg border border-deepseek-border bg-deepseek-sidebar p-1 shadow-md">
        {group.items.map(item => <button type="button" role="menuitem" key={item.label} onClick={() => { item.action(); setExpanded(null); }} className={itemClass}><item.icon size={15} className="shrink-0" />{item.label}</button>)}
      </div>, document.body)}
    </div>)}
  </div>;
  return <nav aria-label={t('sidebar.mainNavigation')} className="space-y-1 mb-4">
    <button type="button" aria-label={t('sidebar.newChat')} title={collapsed ? t('sidebar.newChat') : undefined} onClick={onNewChat} className={itemClass}><MessageSquare size={17} className="shrink-0" />{!collapsed && t('sidebar.newChat')}</button>
    <button type="button" aria-label={t('sidebar.search')} title={collapsed ? t('sidebar.search') : undefined} onClick={onSearch} className={itemClass}><Search size={17} className="shrink-0" />{!collapsed && t('sidebar.search')}</button>
    {toolbar && createPortal(groups, toolbar)}
    {collapsed && <button type="button" aria-label={t('sidebar.projects')} title={t('sidebar.projects')} onClick={() => navigate('/projects')} className={itemClass}><Folder size={17} /></button>}
  </nav>;
}
