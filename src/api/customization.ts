import { request } from './client';

export type PromptMode = 'default' | 'append' | 'replace';

export interface CustomTool {
  name: string;
  description: string;
  input_schema: Record<string, unknown>;
  command: string;
  enabled: boolean;
  timeout_secs?: number | null;
}

export interface Customization {
  system_prompt: { mode: PromptMode; text: string };
  disabled_tools: string[];
  tool_descriptions: Record<string, string>;
  custom_tools: CustomTool[];
}

export interface CustomizationState {
  customization: Customization;
  default_system_prompt: string;
  builtin_tools: { name: string; description: string }[];
}

export const customizationApi = {
  load: async (): Promise<CustomizationState> => (await request('/customization')).json(),
  save: async (customization: Customization): Promise<void> => {
    await request('/customization', { method: 'PUT', body: JSON.stringify(customization) });
  },
};
