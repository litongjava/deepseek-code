import type { PendingFile } from '../../components/FileUploadPreview';
import { useBusySendStore } from '../../stores/useBusySendStore';
import type { UserContent } from '../../utils/userContent';

export async function submitBusyMessage(
  conversationId: string,
  text: string,
  files: PendingFile[],
  behavior: 'queue' | 'steer',
) {
  const blocks: Record<string, any>[] = [];
  if (text) blocks.push({ type: 'text', text });
  for (const file of files) {
    if (file.status !== 'done') throw new Error('Wait for attachments to finish uploading.');
    if (file.fileType === 'image' || file.mimeType.startsWith('image/')) {
      const url = await new Promise<string>((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(String(reader.result));
        reader.onerror = () => reject(new Error(`Cannot read image: ${file.fileName}`));
        reader.readAsDataURL(file.file);
      });
      blocks.push({ type: 'image_url', image_url: { url }, fileName: file.fileName, size: file.file.size });
    } else {
      if (file.source === 'github') throw new Error('Repository attachments cannot be queued. Send a text instruction referencing the repository.');
      const data = await new Promise<string>((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => resolve(String(reader.result).split(',')[1]);
        reader.onerror = () => reject(new Error(`Cannot read attachment: ${file.fileName}`));
        reader.readAsDataURL(file.file);
      });
      blocks.push({ type: 'attachment', fileName: file.fileName, size: file.file.size,
        source: { type: 'base64', media_type: file.mimeType || 'application/octet-stream', data } });
    }
  }
  const content: UserContent = files.length ? blocks : text;
  await useBusySendStore.getState().enqueue(conversationId, content, behavior === 'steer' ? 'steer' : 'next_turn');
}
