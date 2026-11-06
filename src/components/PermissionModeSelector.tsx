import React, { useState, useRef, useEffect, useLayoutEffect } from 'react';
import { createPortal } from 'react-dom';
import { useChatStore } from '../stores/useChatStore';
import { useI18n } from '../hooks/useI18n';
import { Shield, ShieldCheck, ShieldOff, Eye } from 'lucide-react';
import { invoke } from '@tauri-apps/api/core';
import { sessionBashPermission } from '../api';

import type { PermissionMode } from '../stores/useGeneralSettingsStore';

interface PermissionModeOption {
  value: PermissionMode;
  label: string;
  labelZh: string;
  description: string;
  descriptionZh: string;
  icon: React.ReactNode;
}

const MODE_OPTIONS: PermissionModeOption[] = [
  {
    value: 'ask_permissions',
    label: 'Ask permissions',
    labelZh: '询问权限',
    description: 'Allow reads; confirm actions without prior permission',
    descriptionZh: '允许读取，未预先授权的操作需要确认',
    icon: <Shield size={14} />,
  },
  {
    value: 'accept_edits',
    label: 'Accept edits',
    labelZh: '接受编辑',
    description: 'Auto-accept edit operations, confirm dangerous actions',
    descriptionZh: '自动接受编辑操作，危险操作需要确认',
    icon: <ShieldCheck size={14} />,
  },
  {
    value: 'plan_mode',
    label: 'Plan mode',
    labelZh: '计划模式',
    description: 'Read-only mode, all modifications are denied',
    descriptionZh: '只读模式，禁止所有修改操作',
    icon: <Eye size={14} />,
  },
  {
    value: 'dont_ask',
    label: "Don't ask",
    labelZh: '不询问权限',
    description: 'Allow pre-approved actions; deny actions that need permission',
    descriptionZh: '允许已授权操作，拒绝需要询问权限的操作',
    icon: <Shield size={14} />,
  },
  {
    value: 'bypass_permissions',
    label: 'Bypass permissions',
    labelZh: '全托管模式',
    description: 'Approve unconfigured actions; explicit deny and ask rules still apply',
    descriptionZh: '自动允许未配置的操作；仍遵循明确的拒绝和询问规则',
    icon: <ShieldOff size={14} />,
  },
];

interface PermissionModeSelectorProps {
  conversationId?: string | null;
  neutral?: boolean;
  className?: string;
  value?: PermissionMode;
  onChange?: (mode: PermissionMode) => void;
}

export default function PermissionModeSelector({ className = '', value, onChange, neutral = false, conversationId }: PermissionModeSelectorProps) {
  const { t } = useI18n();
  const activePermissionMode = useChatStore((s) => s.permissionMode);
  const permissionMode = value ?? activePermissionMode;
  const setPermissionMode = useChatStore((s) => s.setPermissionMode);
  const [isOpen, setIsOpen] = useState(false);
  const [showBypassWarning, setShowBypassWarning] = useState(false);
  const [modeError, setModeError] = useState('');
  const [changingMode, setChangingMode] = useState(false);
  const [bashAllowed, setBashAllowed] = useState<boolean | null>(null);
  useEffect(() => {
    if (!isOpen || !conversationId) return;
    let active = true;
    setBashAllowed(null);
    sessionBashPermission(conversationId).then(result => { if (active) setBashAllowed(result.allowed); }).catch(error => { if (active) setModeError(String(error)); });
    return () => { active = false; };
  }, [isOpen, conversationId]);
  const dropdownRef = useRef<HTMLDivElement>(null);
  const buttonRef = useRef<HTMLButtonElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [menuStyle, setMenuStyle] = useState<React.CSSProperties>({});

  const currentMode = permissionMode === 'auto'
    ? { label: 'Auto permissions', icon: <ShieldCheck size={14} /> }
    : MODE_OPTIONS.find((m) => m.value === permissionMode) || MODE_OPTIONS[0];

  useEffect(() => {
    if (value !== undefined || !(window as any).__TAURI_INTERNALS__) return;
    let cancelled = false;
    invoke<string>('get_permission_mode').then(mode => {
      if (!cancelled) setPermissionMode(mode);
    }).catch(error => { if (!cancelled) setModeError(String(error)); });
    return () => { cancelled = true; };
  }, [value, setPermissionMode]);

  const applyMode = async (mode: PermissionMode) => {
    if (onChange) { onChange(mode); setIsOpen(false); setShowBypassWarning(false); return; }
    setChangingMode(true);
    setModeError('');
    try {
      await invoke('set_permission_mode', { mode });
      setPermissionMode(mode);
      setIsOpen(false);
      setShowBypassWarning(false);
      if (mode === 'bypass_permissions') localStorage.setItem('permission_bypass_confirmed', 'true');
    } catch (error) {
      setModeError(String(error));
      setShowBypassWarning(false);
      setIsOpen(true);
    } finally { setChangingMode(false); }
  };

  useEffect(() => {
    const handleClickOutside = (e: MouseEvent) => {
      if (!dropdownRef.current?.contains(e.target as Node) && !menuRef.current?.contains(e.target as Node)) {
        setIsOpen(false);
      }
    };
    const handleEscape = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setIsOpen(false);
        buttonRef.current?.focus();
      }
    };
    if (isOpen) {
      document.addEventListener('mousedown', handleClickOutside);
      document.addEventListener('keydown', handleEscape);
    }
    return () => {
      document.removeEventListener('mousedown', handleClickOutside);
      document.removeEventListener('keydown', handleEscape);
    };
  }, [isOpen]);

  useLayoutEffect(() => {
    if (!isOpen) return;
    const updatePosition = () => {
      if (!buttonRef.current || !menuRef.current) return;
      const rect = buttonRef.current.getBoundingClientRect();
      const gap = 8;
      const width = Math.min(288, window.innerWidth - gap * 2);
      const below = Math.max(0, window.innerHeight - rect.bottom - gap * 2);
      const above = Math.max(0, rect.top - gap * 2);
      const height = menuRef.current.scrollHeight;
      const openAbove = height > below && above > below;
      const maxHeight = openAbove ? above : below;
      setMenuStyle({
        width,
        maxHeight,
        left: Math.max(gap, Math.min(rect.left + rect.width / 2 - width / 2, window.innerWidth - width - gap)),
        top: openAbove ? rect.top - gap - Math.min(height, maxHeight) : rect.bottom + gap,
      });
    };
    updatePosition();
    window.addEventListener('resize', updatePosition);
    window.addEventListener('scroll', updatePosition, true);
    return () => {
      window.removeEventListener('resize', updatePosition);
      window.removeEventListener('scroll', updatePosition, true);
    };
  }, [isOpen]);

  const handleModeChange = (mode: PermissionMode) => {
    if (mode === 'bypass_permissions') {
      const hasConfirmed = localStorage.getItem('permission_bypass_confirmed');
      if (!hasConfirmed) {
        setShowBypassWarning(true);
        setIsOpen(false);
        return;
      }
    }
    void applyMode(mode);
  };

  const confirmBypass = () => {
    void applyMode('bypass_permissions');
  };

  const getModeBadgeColor = (mode: PermissionMode) => {
    switch (mode) {
      case 'ask_permissions': return 'bg-claude-textSecondary/20 text-claude-textSecondary';
      case 'accept_edits': return 'bg-green-500/20 text-green-500';
      case 'dont_ask': return 'bg-claude-textSecondary/20 text-claude-textSecondary';
      case 'plan_mode': return 'bg-blue-500/20 text-blue-500';
      case 'bypass_permissions': return 'bg-orange-500/20 text-orange-500';
      default: return 'bg-claude-textSecondary/20 text-claude-textSecondary';
    }
  };

  return (
    <>
      <div className={`relative ${className}`} ref={dropdownRef}>
        <button
          ref={buttonRef}
          aria-expanded={isOpen}
          aria-haspopup="dialog"
          onClick={() => setIsOpen(!isOpen)}
          className="flex items-center gap-1.5 px-2.5 py-1.5 rounded-lg text-[12px] font-medium bg-claude-input border border-claude-border hover:bg-claude-hover transition-colors"
        >
          {currentMode.icon}
          <span className={neutral ? 'text-claude-text' : getModeBadgeColor(permissionMode as PermissionMode)}>{currentMode.label}</span>
          <svg width="10" height="6" viewBox="0 0 10 6" fill="none" className={`text-claude-textSecondary transition-transform ${isOpen ? 'rotate-180' : ''}`}>
            <path d="M1 1L5 5L9 1" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round" />
          </svg>
        </button>

        {isOpen && createPortal(
          <div
            ref={menuRef}
            role="dialog"
            aria-label="Permission Mode"
            className="fixed z-[120] w-72 rounded-xl shadow-xl border border-claude-border bg-claude-input overflow-y-auto"
            style={menuStyle}
          >
            <div className="px-3 py-2 border-b border-claude-border">
              <span className="text-[11px] font-medium text-claude-textSecondary uppercase tracking-wide">
                Permission Mode
              </span>
            </div>
            <div className="p-1.5">
              {modeError && <p role="alert" className="px-3 py-2 text-[12px] text-claude-textSecondary break-words">{modeError}</p>}
              {MODE_OPTIONS.map((option) => (
                <button
                  key={option.value}
                  disabled={changingMode}
                  onClick={() => handleModeChange(option.value)}
                  className={`w-full text-left px-3 py-2.5 rounded-lg transition-colors flex items-start gap-2.5 ${
                    permissionMode === option.value
                      ? 'bg-claude-btn-hover'
                      : 'hover:bg-claude-hover'
                  }`}
                >
                  <div className={`mt-0.5 flex-shrink-0 ${permissionMode === option.value ? 'text-claude-text' : 'text-claude-textSecondary'}`}>
                    {option.icon}
                  </div>
                  <div className="flex-1 min-w-0">
                    <div className={`text-[13px] font-medium ${permissionMode === option.value ? 'text-claude-text' : 'text-claude-textSecondary'}`}>
                      {option.label}
                    </div>
                    <div className="text-[11px] text-claude-textSecondary/70 mt-0.5 leading-tight">
                      {option.description}
                    </div>
                  </div>
                  {permissionMode === option.value && (
                    <div className="flex-shrink-0 mt-1">
                      <svg width="14" height="14" viewBox="0 0 14 14" fill="none">
                        <path d="M2 7L5.5 10.5L12 3.5" stroke={neutral ? 'currentColor' : '#387ee0'} strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" />
                      </svg>
                    </div>
                  )}
                </button>
              ))}
              {conversationId && <div className="border-t border-claude-border px-3 py-2 text-xs text-claude-textSecondary">
                <p>Session Bash: {bashAllowed == null ? 'Loading…' : bashAllowed ? 'Allowed · dangerous commands excluded' : 'Not granted'}</p>
                {bashAllowed && <button className="mt-2 underline" disabled={changingMode} onClick={async () => {
                  setChangingMode(true);
                  try { setBashAllowed((await sessionBashPermission(conversationId, true)).allowed); }
                  catch (error) { setModeError(String(error)); }
                  finally { setChangingMode(false); }
                }}>Revoke session Bash permission</button>}
              </div>}
            </div>
          </div>,
          document.body
        )}
      </div>

      {showBypassWarning && (
        <div className="fixed inset-0 z-[100] flex items-center justify-center bg-black/50" onClick={() => setShowBypassWarning(false)}>
          <div
            className="bg-claude-input rounded-2xl shadow-xl w-[420px] p-6 animate-fade-in border border-claude-border"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-center gap-3 mb-4">
              <div className="w-10 h-10 rounded-full bg-orange-500/20 flex items-center justify-center flex-shrink-0">
                <ShieldOff size={20} className="text-orange-500" />
              </div>
              <div>
                <h3 className="text-[16px] font-semibold text-claude-text">
                  启用全托管模式？
                </h3>
                <p className="text-[13px] text-claude-textSecondary mt-0.5">
                  Enable Bypass Permissions mode?
                </p>
              </div>
            </div>
            <div className="bg-orange-500/10 border border-orange-500/20 rounded-lg p-3 mb-5">
              <p className="text-[13px] text-orange-400 leading-relaxed">
                在此模式下，所有工具调用（包括文件写入、命令执行等）都将自动执行，无需您的确认。请确保您信任当前对话中的 AI 行为。
              </p>
              <p className="text-[12px] text-orange-400/70 mt-2">
                In this mode, all tool calls (including file writes, command execution, etc.) will be executed automatically without your confirmation.
              </p>
            </div>
            <div className="flex justify-end gap-3">
              <button
                onClick={() => setShowBypassWarning(false)}
                className="px-4 py-2 text-[13px] font-medium text-claude-text bg-claude-btn-hover hover:bg-claude-hover rounded-lg transition-colors"
              >
                取消
              </button>
              <button
                onClick={confirmBypass}
                disabled={changingMode}
                className="px-4 py-2 text-[13px] font-medium text-white bg-orange-600 hover:bg-orange-700 rounded-lg transition-colors"
              >
                确认启用
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  );
}
