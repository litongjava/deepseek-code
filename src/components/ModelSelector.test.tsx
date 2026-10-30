import { fireEvent, render, screen, within } from '@testing-library/react';
import { expect, it, vi } from 'vitest';
import ModelSelector from './ModelSelector';
import { useUIStore } from '../stores/useUIStore';
import en from '../locales/en.json';

it('orders models by name ascending within each provider without changing the source list', () => {
  const names = ['gpt-10', 'claude-sonnet-4-6', 'gpt-2', 'Gemini-3'];
  const models = names.map(name => ({ id: `provider::${name}`, name, enabled: 1, providerId: 'provider', providerName: 'Provider' }));
  const onChange = vi.fn();
  useUIStore.getState().setLanguage('en');
  render(<ModelSelector models={models} currentModelString={models[0].id} onModelChange={onChange} />);
  fireEvent.click(screen.getByRole('button', { name: en.models.modelAndEffort }));
  const menu = within(screen.getByRole('dialog', { name: '模型选择' }));
  fireEvent.click(menu.getByRole('button', { name: /^Model / }));
  const rows = menu.getAllByRole('button').filter(button => names.includes(button.textContent || ''));
  expect(rows.map(button => button.textContent)).toEqual(['claude-sonnet-4-6', 'Gemini-3', 'gpt-2', 'gpt-10']);
  expect(models.map(model => model.name)).toEqual(names);
  fireEvent.click(rows[2]);
  expect(onChange).toHaveBeenCalledWith('provider::gpt-2');
});
