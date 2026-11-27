import React from 'react';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import SearchStartupReminder, { isSearchConfigured, WEB_SEARCH_REMINDER_LINE } from './SearchStartupReminder';
import * as api from '../api';

vi.mock('../api', () => ({
  getSearchSettings: vi.fn(),
}));

describe('SearchStartupReminder', () => {
  beforeEach(() => {
    vi.clearAllMocks();
  });

  it('requires baseUrl, model, and hasApiKey to be configured', () => {
    expect(isSearchConfigured({ baseUrl: 'https://api.deepseek.com/anthropic/v1', model: 'deepseek-flash', hasApiKey: true })).toBe(true);
    expect(isSearchConfigured({ baseUrl: 'https://api.deepseek.com/anthropic/v1', model: 'deepseek-flash', hasApiKey: false })).toBe(false);
    expect(isSearchConfigured({ baseUrl: '   ', model: 'deepseek-flash', hasApiKey: true })).toBe(false);
    expect(isSearchConfigured({ baseUrl: 'https://api.deepseek.com/anthropic/v1', model: '', hasApiKey: true })).toBe(false);
  });

  it('shows System Reminder when web search is missing any required field and navigates to settings', async () => {
    vi.mocked(api.getSearchSettings).mockResolvedValue({
      baseUrl: 'https://api.deepseek.com/anthropic/v1',
      model: 'deepseek-flash',
      hasApiKey: false,
    });

    render(
      <MemoryRouter initialEntries={['/']}>
        <SearchStartupReminder />
        <Routes>
          <Route path="/" element={<div>Home</div>} />
          <Route path="/settings" element={<div>Settings Page</div>} />
        </Routes>
      </MemoryRouter>
    );

    expect(await screen.findByText('System Reminder')).toBeTruthy();
    expect(screen.getByText(`- ${WEB_SEARCH_REMINDER_LINE}`)).toBeTruthy();

    fireEvent.click(screen.getByRole('button', { name: 'Configure' }));
    expect(await screen.findByText('Settings Page')).toBeTruthy();
  });

  it('stays hidden when baseUrl, model, and apiKey are all configured', async () => {
    vi.mocked(api.getSearchSettings).mockResolvedValue({
      baseUrl: 'https://api.openai.com/v1',
      model: 'gpt-5-mini',
      hasApiKey: true,
    });

    render(
      <MemoryRouter initialEntries={['/']}>
        <SearchStartupReminder />
      </MemoryRouter>
    );

    await waitFor(() => expect(api.getSearchSettings).toHaveBeenCalledTimes(1));
    expect(screen.queryByText('System Reminder')).toBeNull();
  });

  it('hides when dismissed', async () => {
    vi.mocked(api.getSearchSettings).mockResolvedValue({
      baseUrl: '',
      model: 'deepseek-flash',
      hasApiKey: true,
    });

    render(
      <MemoryRouter initialEntries={['/']}>
        <SearchStartupReminder />
      </MemoryRouter>
    );

    expect(await screen.findByText('System Reminder')).toBeTruthy();
    fireEvent.click(screen.getByRole('button', { name: 'Dismiss reminder' }));
    expect(screen.queryByText('System Reminder')).toBeNull();
  });
});
