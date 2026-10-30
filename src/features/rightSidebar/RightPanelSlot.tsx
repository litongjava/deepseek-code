import React, { useEffect, useState } from 'react';
import CodePanel from '../../components/CodePanel';
import ArtifactsPanel from '../../components/ArtifactsPanel';
import DraggableDivider from '../../components/DraggableDivider';
import type { DocumentInfo } from '../../components/DocumentCard';
import { useI18n } from '../../hooks/useI18n';
import { clampRightSidebarWidth, useRightSidebarStore } from '../../stores/useRightSidebarStore';
import RightSidebar from './RightSidebar';

interface RightPanelSlotProps {
  containerRef: React.RefObject<HTMLDivElement | null>;
  documentPanelDoc: DocumentInfo | null;
  documentPanelWidth: number;
  onDocumentPanelResize: (percent: number) => void;
  onCloseDocument: () => void;
  showArtifacts: boolean;
  artifacts: DocumentInfo[];
  onCloseArtifacts: () => void;
  onOpenDocument: (doc: DocumentInfo) => void;
  /** Visibility from useRightSidebarController; already false while a document or artifacts panel is shown. */
  sidebarVisible: boolean;
  onCloseSidebar: () => void;
}

/** The single right-hand panel next to the chat: document preview, artifacts list, or the right sidebar. */
export default function RightPanelSlot({
  containerRef, documentPanelDoc, documentPanelWidth, onDocumentPanelResize, onCloseDocument,
  showArtifacts, artifacts, onCloseArtifacts, onOpenDocument, sidebarVisible, onCloseSidebar,
}: RightPanelSlotProps) {
  const { t } = useI18n();
  const storedWidth = useRightSidebarStore(s => s.width);
  const setSidebarWidth = useRightSidebarStore(s => s.setWidth);
  const [resizing, setResizing] = useState(false);
  const [viewportWidth, setViewportWidth] = useState(() => window.innerWidth);
  const showSidebar = sidebarVisible && !documentPanelDoc && !showArtifacts;
  const visible = !!documentPanelDoc || showArtifacts || showSidebar;

  useEffect(() => {
    if (!showSidebar) return;
    const handleResize = () => setViewportWidth(window.innerWidth);
    window.addEventListener('resize', handleResize);
    return () => window.removeEventListener('resize', handleResize);
  }, [showSidebar]);

  const width = documentPanelDoc ? `${documentPanelWidth}%`
    : showArtifacts ? '360px'
      : showSidebar ? `${clampRightSidebarWidth(storedWidth, viewportWidth)}px`
        : '0px';

  return (
    <div
      data-testid="right-panel-slot"
      className={`h-full bg-claude-bg ${resizing ? '' : 'transition-all duration-300 ease-out'} flex z-20 relative ${visible ? 'border-l border-claude-border' : 'pointer-events-none'}`}
      style={{ width, opacity: visible ? 1 : 0, overflow: 'hidden' }}
    >
      {documentPanelDoc && (
        <div className="absolute left-0 top-0 bottom-0 h-full z-50">
          <DraggableDivider onResize={onDocumentPanelResize} containerRef={containerRef} />
        </div>
      )}
      {showSidebar && (
        <div className="absolute left-0 top-0 bottom-0 h-full z-50">
          <DraggableDivider unit="px" onResize={setSidebarWidth} containerRef={containerRef} ariaLabel={t('rightSidebar.resize')} onDraggingChange={setResizing} />
        </div>
      )}
      <div className="w-full h-full flex relative min-w-0 overflow-hidden">
        {documentPanelDoc ? (
          <CodePanel document={documentPanelDoc} onClose={onCloseDocument} />
        ) : showArtifacts ? (
          <ArtifactsPanel documents={artifacts} onClose={onCloseArtifacts} onOpenDocument={onOpenDocument} />
        ) : showSidebar ? (
          <RightSidebar onClose={onCloseSidebar} />
        ) : null}
      </div>
    </div>
  );
}
