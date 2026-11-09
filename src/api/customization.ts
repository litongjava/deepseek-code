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

/** The two independent switches of one built-in tool. */
export interface ToolLoading {
  enabled: boolean;
  on_demand: boolean;
}

export interface Customization {
  system_prompt: { mode: PromptMode; text: string };
  /** Legacy entries. A built-in name here is normalized on the first save. */
  disabled_tools: string[];
  tool_descriptions: Record<string, string>;
  custom_tools: CustomTool[];
  tool_loading: Record<string, ToolLoading>;
}

export interface BuiltinTool {
  name: string;
  description: string;
  /** In the default core set; both switches stay editable. */
  core: boolean;
  default_enabled: boolean;
  default_on_demand: boolean;
  /** Effective state, already resolved from the saved file and the legacy list. */
  enabled: boolean;
  on_demand: boolean;
}

export interface CustomizationState {
  customization: Customization;
  default_system_prompt: string;
  builtin_tools: BuiltinTool[];
}

export type CustomizationSection = 'prompts' | 'tools';

export const customizationApi = {
  load: async (): Promise<CustomizationState> => (await request('/customization')).json(),
  // Only the given half is written; the server returns the normalized file so
  // the page and the disk agree.
  save: async (customization: Customization, section: CustomizationSection): Promise<Customization> => {
    const response = await request(`/customization?section=${section}`, { method: 'PUT', body: JSON.stringify(customization) });
    const body = await response.json().catch(() => null);
    return (body?.customization as Customization | undefined) ?? customization;
  },
};
