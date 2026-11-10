import { useCallback, useEffect, useState } from 'react';
import { ArrowUp, Folder, Home, Loader2 } from 'lucide-react';
import Modal from '../../components/ui/Modal';
import { useI18n } from '../../hooks/useI18n';
import { listDirectories, type DirectoryListing } from '../../api/directories';
import { describeError } from '../../utils/networkError';

/**
 * Browses folders on the machine running the engine, one level per request,
 * for browsers and the mobile app where no native folder dialog exists.
 */
export default function FolderPicker({ isOpen, onClose, onSelect, initialPath }: {
  isOpen: boolean;
  onClose: () => void;
  onSelect: (path: string) => void;
  initialPath?: string;
}) {
  const { t } = useI18n();
  const [listing, setListing] = useState<DirectoryListing | null>(null);
  const [typed, setTyped] = useState('');
  const [hidden, setHidden] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');

  const load = useCallback(async (path?: string, showHidden = hidden) => {
    setLoading(true);
    setError('');
    try {
      const next = await listDirectories(path, showHidden);
      setListing(next);
      setTyped(next.path);
    } catch (failure) {
      setError(describeError(failure));
    } finally {
      setLoading(false);
    }
  }, [hidden]);

  useEffect(() => {
    if (isOpen) void load(initialPath || undefined);
    // Each opening starts from the given folder, not the last one browsed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isOpen]);

  const choose = () => {
    if (!listing) return;
    onSelect(listing.path);
    onClose();
  };

  return (
    <Modal isOpen={isOpen} onClose={onClose} title={t('folderPicker.title')} size="md" className="max-w-[94vw]" layerClassName="z-[300]">
      <div className="flex flex-col gap-3 text-sm text-deepseek-text">
        {/* React events bubble through the portal: keep this submit from reaching the project form. */}
        <form className="flex gap-2" onSubmit={event => { event.preventDefault(); event.stopPropagation(); if (typed.trim()) void load(typed.trim()); }}>
          <input
            aria-label={t('projectDialogs.folderPath')}
            value={typed}
            onChange={event => setTyped(event.target.value)}
            className="min-w-0 flex-1 rounded-lg border border-deepseek-border bg-transparent px-3 py-2 outline-none focus:border-blue-500"
          />
          <button type="submit" disabled={loading || !typed.trim()} className="rounded-lg bg-deepseek-hover px-3 disabled:opacity-40">{t('folderPicker.open')}</button>
        </form>
        <div className="flex flex-wrap items-center gap-2">
          <button type="button" disabled={loading || !listing?.parent} onClick={() => void load(listing!.parent!)} className="flex items-center gap-1 rounded-lg bg-deepseek-hover px-2.5 py-1.5 disabled:opacity-40">
            <ArrowUp size={14} /> {t('folderPicker.up')}
          </button>
          <button type="button" disabled={loading || !listing?.home} onClick={() => void load(listing!.home!)} className="flex items-center gap-1 rounded-lg bg-deepseek-hover px-2.5 py-1.5 disabled:opacity-40">
            <Home size={14} /> {t('folderPicker.home')}
          </button>
          {(listing?.roots.length ?? 0) > 1 && listing!.roots.map(root => (
            <button key={root} type="button" disabled={loading} onClick={() => void load(root)} className="rounded-lg bg-deepseek-hover px-2.5 py-1.5">{root}</button>
          ))}
          <label className="ml-auto flex items-center gap-1.5 text-deepseek-textSecondary">
            <input type="checkbox" checked={hidden} onChange={event => { setHidden(event.target.checked); void load(listing?.path, event.target.checked); }} />
            {t('folderPicker.hidden')}
          </label>
        </div>
        <div role="listbox" aria-label={t('folderPicker.title')} className="h-72 max-h-[50vh] overflow-y-auto rounded-xl border border-deepseek-border">
          {loading && !listing && <p className="flex items-center gap-2 p-3 text-deepseek-textSecondary"><Loader2 size={14} className="animate-spin" /> {t('folderPicker.loading')}</p>}
          {listing && listing.directories.length === 0 && <p className="p-3 text-deepseek-textSecondary">{t('folderPicker.empty')}</p>}
          {listing?.directories.map(directory => (
            <button
              key={directory.path}
              type="button"
              role="option"
              aria-selected={false}
              disabled={loading}
              title={directory.path}
              onClick={() => void load(directory.path)}
              className="flex w-full items-center gap-2 px-3 py-2 text-left hover:bg-deepseek-hover disabled:opacity-60"
            >
              <Folder size={15} className="shrink-0 text-deepseek-textSecondary" />
              <span className="min-w-0 truncate">{directory.name}</span>
            </button>
          ))}
          {!!listing?.truncated && <p className="p-3 text-deepseek-textSecondary">{t('folderPicker.more', { count: listing.truncated })}</p>}
        </div>
        {error && <p role="alert" className="text-red-500">{error}</p>}
        <div className="flex items-center justify-between gap-3">
          <span className="min-w-0 truncate text-deepseek-textSecondary" title={listing?.path}>{listing?.path}</span>
          <button type="button" disabled={loading || !listing} onClick={choose} className="shrink-0 rounded-lg bg-deepseek-accent px-4 py-2 text-white disabled:opacity-40">
            {t('folderPicker.select')}
          </button>
        </div>
      </div>
    </Modal>
  );
}
