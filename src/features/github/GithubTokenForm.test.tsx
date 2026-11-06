import { beforeEach, expect, it, vi } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';

const connectGithubToken = vi.hoisted(() => vi.fn());
vi.mock('../../api/github', () => ({ connectGithubToken }));
import GithubTokenForm from './GithubTokenForm';

beforeEach(() => { connectGithubToken.mockReset(); localStorage.setItem('app_language', 'en'); });

it('connects with a trimmed token and reports a rejected one', async () => {
  const onConnected = vi.fn();
  render(<GithubTokenForm onConnected={onConnected} />);
  const input = screen.getByLabelText('Personal access token');
  connectGithubToken.mockRejectedValueOnce(new Error('GitHub rejected the token'));
  fireEvent.change(input, { target: { value: 'bad' } });
  fireEvent.click(screen.getByRole('button', { name: 'Connect' }));
  expect(await screen.findByRole('alert')).toHaveTextContent('GitHub rejected this token.');
  expect(onConnected).not.toHaveBeenCalled();

  connectGithubToken.mockResolvedValueOnce({ connected: true, user: { login: 'octo', avatar_url: 'a' } });
  fireEvent.change(input, { target: { value: '  ghp_good  ' } });
  fireEvent.click(screen.getByRole('button', { name: 'Connect' }));
  await waitFor(() => expect(onConnected).toHaveBeenCalledWith({ login: 'octo', avatar_url: 'a' }));
  expect(connectGithubToken).toHaveBeenLastCalledWith('ghp_good');
});
