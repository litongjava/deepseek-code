import type { LucideIcon } from 'lucide-react';

export default function RightSidebarEmptyState({ icon: Icon, title, description }: {
  icon: LucideIcon;
  title: string;
  description: string;
}) {
  return (
    <div className="flex h-full flex-col items-center justify-center gap-3 px-8 text-center">
      <div className="flex h-10 w-10 items-center justify-center rounded-xl bg-claude-hover text-claude-textSecondary">
        <Icon size={20} strokeWidth={1.5} aria-hidden="true" />
      </div>
      <p className="text-[14px] font-medium text-claude-text">{title}</p>
      <p className="max-w-[260px] text-[13px] leading-5 text-claude-textSecondary">{description}</p>
    </div>
  );
}
