import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { beforeEach, expect, it, vi } from 'vitest';
import CodePanel from './CodePanel';
import { getFileSystemTree, readFileContent } from '../api';

vi.mock('../api', () => ({ getFileSystemTree: vi.fn(), readFileContent: vi.fn(), writeFileContent: vi.fn() }));
vi.mock('./MarkdownRenderer', () => ({ default: ({ content }: { content: string }) => <div>{content}</div> }));
vi.mock('./SlidePreview', () => ({ default: () => null }));
vi.mock('./DocxPreview', () => ({ default: () => null }));
vi.mock('./PdfPreview', () => ({ default: () => null }));

const doc = { id: 'plan', title: 'plan.md', filename: 'plan.md', url: '', format: 'markdown', content: '# Reviewed plan' };
beforeEach(() => { vi.resetAllMocks(); });

it('opens the document alone and only loads the tree when requested, retaining it across toggles', async () => {
  vi.mocked(getFileSystemTree).mockResolvedValue({ success: true, path: '/project', tree: [{ name: 'notes.md', path: '/project/notes.md', is_dir: false, size: 12 }] });
  vi.mocked(readFileContent).mockResolvedValue({ success: true, content: '# Project notes', path: '/project/notes.md' });
  render(<CodePanel document={doc} onClose={vi.fn()} />);
  expect(screen.getByText('# Reviewed plan')).toBeVisible();
  expect(screen.queryByText('Explorer')).not.toBeInTheDocument();
  expect(getFileSystemTree).not.toHaveBeenCalled();
  fireEvent.click(screen.getByRole('button', { name: 'Show file tree' }));
  fireEvent.click(await screen.findByText('notes.md'));
  expect(await screen.findByText('# Project notes')).toBeVisible();
  expect(readFileContent).toHaveBeenCalledWith('/project/notes.md');
  fireEvent.click(screen.getByRole('button', { name: 'Hide file tree' }));
  expect(screen.queryByRole('region', { name: 'Document file tree' })).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole('button', { name: 'Show file tree' }));
  expect(getFileSystemTree).toHaveBeenCalledTimes(1);
  expect(screen.getByText('# Project notes')).toBeVisible();
});

it('keeps the preview readable after a tree failure and permits a real retry', async () => {
  vi.mocked(getFileSystemTree).mockRejectedValueOnce(new Error('Directory unavailable'))
    .mockResolvedValueOnce({ success: true, path: '/project', tree: [] });
  render(<CodePanel document={doc} onClose={vi.fn()} />);
  fireEvent.click(screen.getByRole('button', { name: 'Show file tree' }));
  expect(await screen.findByText('Directory unavailable')).toBeVisible();
  expect(screen.getByText('# Reviewed plan')).toBeVisible();
  fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
  await waitFor(() => expect(screen.getByText('No files loaded')).toBeVisible());
  expect(getFileSystemTree).toHaveBeenCalledTimes(2);
});
