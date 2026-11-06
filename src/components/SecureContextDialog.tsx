import Modal from './ui/Modal';
import { useI18n } from '../hooks/useI18n';
import { useSecureContextNotice } from '../platform/secureContext';

/** Explains why a browser feature is unavailable on a page served over plain HTTP. */
export default function SecureContextDialog() {
  const { t } = useI18n();
  const feature = useSecureContextNotice((state) => state.feature);
  const close = useSecureContextNotice((state) => state.close);
  return (
    <Modal isOpen={feature !== null} onClose={close} title={t('secureContext.title')} size="sm" className="max-w-[92vw]">
      <div role="alert" className="text-sm text-claude-text">
        <p>{feature ? t(`secureContext.${feature}`) : ''}</p>
        <p className="mt-2 text-claude-textSecondary">{t('secureContext.hint')}</p>
        <div className="mt-5 flex justify-end">
          <button type="button" onClick={close} className="rounded-lg bg-claude-accent px-4 py-2 text-white">
            {t('common.close')}
          </button>
        </div>
      </div>
    </Modal>
  );
}
