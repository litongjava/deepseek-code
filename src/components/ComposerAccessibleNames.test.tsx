import { render, screen } from '@testing-library/react';
import { MemoryRouter, Route, Routes } from 'react-router-dom';
import { beforeEach, describe, expect, it, vi } from 'vitest';
import App from '../App';
import MainContent from './MainContent';
import ModelSelector from './ModelSelector';
import VoiceInput from './VoiceInput';
import { getStreamStatus } from '../api';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';

vi.mock('../api', async importOriginal => {
  const actual = await importOriginal<Record<string, unknown>>();
  // Pending promises keep background requests from resolving into unrelated UI state.
  return Object.fromEntries(Object.entries(actual).map(([name, value]) => [name, typeof value === 'function' ? vi.fn(() => new Promise(() => {})) : value]));
});

beforeEach(() => {
  localStorage.clear();
  useUIStore.getState().setLanguage('en');
});

describe('home and composer controls with the English interface', () => {
  it('names the title bar Help button in English', async () => {
    window.location.hash = '#/';
    render(<App />);
    expect(await screen.findByRole('button', { name: en.common.help })).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: '帮助' })).not.toBeInTheDocument();
  });

  it('names the model selector and its empty placeholder in English', () => {
    render(<ModelSelector models={[]} currentModelString="" onModelChange={vi.fn()} />);
    const button = screen.getByRole('button', { name: en.models.modelAndEffort });
    expect(button).toHaveTextContent(en.models.selectModel);
  });

  it('names the Stop generation button while a response is streaming', async () => {
    Element.prototype.scrollTo = vi.fn();
    vi.mocked(getStreamStatus).mockResolvedValue({ active: true, eventCount: 0 });
    render(<MemoryRouter initialEntries={['/chat/conversation-1']}>
      <Routes><Route path="/chat/:id" element={<MainContent onNewChat={vi.fn()} />} /></Routes>
    </MemoryRouter>);
    const stop = await screen.findByRole('button', { name: en.chat.stopGenerating });
    expect(stop).toHaveAttribute('title', en.chat.stopGenerating);
  });

  it('names the voice input dialog and close button in English', () => {
    render(<VoiceInput isOpen onResult={vi.fn()} onClose={vi.fn()} />);
    expect(screen.getByRole('dialog', { name: en.chat.voiceInput })).toBeInTheDocument();
    expect(screen.getByRole('heading', { name: en.chat.voiceInput })).toBeInTheDocument();
    expect(screen.getByRole('button', { name: en.chat.closeVoiceInput })).toBeInTheDocument();
  });
});
