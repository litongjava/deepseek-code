import { FolderOpen } from 'lucide-react';
import { useI18n } from '../../hooks/useI18n';
import RightSidebarEmptyState from './RightSidebarEmptyState';

// Placeholder body; the workspace file view replaces this component.
export default function RightSidebarFilesTab() {
  const { t } = useI18n();
  return <RightSidebarEmptyState icon={FolderOpen} title={t('rightSidebar.filesEmptyTitle')} description={t('rightSidebar.filesEmptyDescription')} />;
}
