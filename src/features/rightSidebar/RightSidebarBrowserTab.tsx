import { Globe } from 'lucide-react';
import { useI18n } from '../../hooks/useI18n';
import RightSidebarEmptyState from './RightSidebarEmptyState';

// Placeholder body; the embedded browser view replaces this component.
export default function RightSidebarBrowserTab() {
  const { t } = useI18n();
  return <RightSidebarEmptyState icon={Globe} title={t('rightSidebar.browserEmptyTitle')} description={t('rightSidebar.browserEmptyDescription')} />;
}
