import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { afterEach, describe, expect, it, vi } from 'vitest';
import FolderFields from './FolderFields';
import { listDirectories } from '../../api/directories';
import en from '../../locales/en.json';

vi.mock('../../api/directories', () => ({ listDirectories: vi.fn() }));
vi.mock('../../services/bridgeConnection', () => ({ hasNativeDesktop: () => false }));

const listing = (path: string, names: string[], parent: string | null = '/home') => ({
  path, parent, home: '/home/dev', roots: ['/'], truncated: 0,
  directories: names.map(name => ({ name, path: `${path}/${name}` })),
});

afterEach(() => vi.resetAllMocks());

describe('folder picker in a browser', () => {
  it('browses server folders and adds the chosen one', async () => {
    vi.mocked(listDirectories).mockImplementation(async path => path === '/home/dev/code'
      ? listing('/home/dev/code', ['app']) : listing('/home/dev', ['code', 'docs']));
    const onChange = vi.fn();
    render(<FolderFields folders={[]} onChange={onChange} />);
    fireEvent.click(screen.getByRole('button', { name: en.projectDialogs.browseFolders }));
    fireEvent.click(await screen.findByRole('option', { name: 'code' }));
    await screen.findByRole('option', { name: 'app' });
    expect(listDirectories).toHaveBeenLastCalledWith('/home/dev/code', false);
    fireEvent.click(screen.getByRole('button', { name: en.folderPicker.select }));
    expect(onChange).toHaveBeenCalledWith(['/home/dev/code']);
    await waitFor(() => expect(screen.queryByRole('listbox')).toBeNull());
  });

  it('opens a typed path, goes up and reports errors readably', async () => {
    vi.mocked(listDirectories).mockImplementation(async path => {
      if (path === '/missing') throw new Error('Folder not found: /missing');
      if (path === '/home') return listing('/home', ['dev'], '/');
      return listing('/home/dev', []);
    });
    render(<FolderFields folders={[]} onChange={vi.fn()} />);
    fireEvent.click(screen.getByRole('button', { name: en.projectDialogs.browseFolders }));
    expect(await screen.findByText(en.folderPicker.empty)).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: en.folderPicker.up }));
    await screen.findByRole('option', { name: 'dev' });
    const inputs = screen.getAllByRole('textbox', { name: en.projectDialogs.folderPath });
    const input = inputs[inputs.length - 1];
    fireEvent.change(input, { target: { value: '/missing' } });
    fireEvent.click(screen.getByRole('button', { name: en.folderPicker.open }));
    expect(await screen.findByText('Folder not found: /missing')).toBeTruthy();
  });
});

describe('folder picker layering', () => {
  it('sits above project dialogs and keeps Escape to itself', async () => {
    vi.mocked(listDirectories).mockResolvedValue(listing('/home/dev', ['code']));
    const outerEscape = vi.fn();
    render(<div onKeyDown={event => { if (event.key === 'Escape') outerEscape(); }}>
      <FolderFields folders={[]} onChange={vi.fn()} />
    </div>);
    fireEvent.click(screen.getByRole('button', { name: en.projectDialogs.browseFolders }));
    const option = await screen.findByRole('option', { name: 'code' });
    const layer = option.closest('[data-modal-layer]')!;
    expect(layer.className).toContain('z-[300]');
    fireEvent.keyDown(option, { key: 'Escape' });
    expect(outerEscape).not.toHaveBeenCalled();
    await waitFor(() => expect(screen.queryByRole('listbox')).toBeNull());
  });
});
