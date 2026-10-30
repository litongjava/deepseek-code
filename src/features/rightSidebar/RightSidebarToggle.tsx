import { PanelRight, PanelRightClose } from 'lucide-react';
import Tooltip from '../../components/ui/Tooltip';
import { useI18n } from '../../hooks/useI18n';
import { RIGHT_SIDEBAR_ID } from './RightSidebar';

export default function RightSidebarToggle({ expanded, onToggle, shortcutLabel = 'Ctrl+Alt+B' }: {
  expanded: boolean;
  onToggle: () => void;
  shortcutLabel?: string;
}) {
  const { t } = useI18n();
  const Icon = expanded ? PanelRightClose : PanelRight;
  return (
    <Tooltip content={`${t(expanded ? 'rightSidebar.hide' : 'rightSidebar.show')} (${shortcutLabel})`} position="bottom">
      <button
        type="button"
        aria-label={t('rightSidebar.toggle')}
        aria-expanded={expanded}
        aria-controls={expanded ? RIGHT_SIDEBAR_ID : undefined}
        aria-keyshortcuts={shortcutLabel.startsWith('Ctrl') ? 'Control+Alt+B' : 'Meta+Alt+B'}
        onClick={onToggle}
        className={`p-1.5 rounded-md transition-colors hover:bg-black/5 dark:hover:bg-white/5 ${expanded ? 'text-claude-text' : 'text-claude-textSecondary hover:text-claude-text'}`}
      >
        <Icon size={16} strokeWidth={1.5} />
      </button>
    </Tooltip>
  );
}
