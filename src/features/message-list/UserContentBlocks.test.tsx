import { afterEach, expect, it } from 'vitest';
import { cleanup, render, screen } from '@testing-library/react';
import UserContentBlocks from './UserContentBlocks';

afterEach(cleanup);
it('restores a downloadable file from persisted bytes without showing base64 as text', () => {
  render(<UserContentBlocks content={JSON.stringify([{ type: 'attachment', fileName: 'notes.txt', size: 3,
    source: { type: 'base64', media_type: 'text/plain', data: 'YWJj' } }])} />);
  const link = screen.getByRole('link', { name: 'Download notes.txt' });
  expect(link.getAttribute('href')).toBe('data:application/octet-stream;base64,YWJj');
  expect(link.getAttribute('download')).toBe('notes.txt');
  expect(screen.getByText('3 B')).toBeTruthy();
  expect(screen.queryByText('YWJj')).toBeNull();
});
