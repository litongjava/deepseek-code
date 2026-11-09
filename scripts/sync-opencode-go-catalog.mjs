// Refresh the built-in `opencode-go` catalog entry from OpenCode's public model
// list. The API key is read from the environment and never written to a file.
//
//   OPENCODE_API_KEY=... node scripts/sync-opencode-go-catalog.mjs
import fs from 'node:fs';

const key = process.env.OPENCODE_API_KEY;
if (!key) throw new Error('Set OPENCODE_API_KEY to list the OpenCode Go models');

const MODELS_URL = 'https://opencode.ai/inference/go/openai/v1/models';
const CATALOG = 'src/data/providerCatalog.json';
const PROVIDER_ID = 'opencode-go';

// OpenCode's transport name -> the protocol name used by the catalog.
const PROTOCOLS = {
  'chat-completions': 'openai-completions',
  responses: 'openai-responses',
  messages: 'anthropic-messages',
  'generate-content': 'google-generative-ai',
};
const EFFORT_LEVELS = ['off', 'minimal', 'low', 'medium', 'high', 'xhigh', 'max'];

function inputTypes(model) {
  const inputs = model.modalities?.input || ['text'];
  const types = [];
  if (inputs.includes('text')) types.push('text');
  if (inputs.some(type => type.startsWith('image'))) types.push('image');
  return types.length ? types : ['text'];
}

function thinkingLevelMap(model) {
  const effort = model.capabilities?.reasoning?.effort;
  if (!Array.isArray(effort) || effort.length === 0) return undefined;
  return Object.fromEntries(
    EFFORT_LEVELS.map(level => [level, effort.includes(level) ? level : null]),
  );
}

function compatFor(id, family, api) {
  if (api === 'openai-completions') {
    const compat = { supportsStore: false, supportsDeveloperRole: false, maxTokensField: 'max_tokens' };
    if (id.startsWith('deepseek') || (family || '').startsWith('deepseek')) {
      compat.requiresReasoningContentOnAssistantMessages = true;
      compat.thinkingFormat = 'deepseek';
    }
    return compat;
  }
  if (api === 'openai-responses') return { sessionAffinityFormat: 'openai-nosession' };
  return undefined;
}

function convert(model, previous) {
  const entry = Object.entries(model.api || {}).find(([name]) => PROTOCOLS[name]);
  if (!entry) throw new Error(`${model.id}: no supported API transport in ${JSON.stringify(model.api)}`);
  const [transport, api] = entry;
  const converted = {
    id: model.id,
    name: model.name || model.id,
    enabled: true,
    contextWindow: model.limit?.context,
    maxTokens: model.limit?.output,
    inputTypes: inputTypes(model),
    api: PROTOCOLS[transport],
    reasoning: model.capabilities?.reasoning?.supported === true,
    baseUrl: api.base_url,
  };
  if (converted.contextWindow == null) delete converted.contextWindow;
  if (converted.maxTokens == null) delete converted.maxTokens;
  const levels = thinkingLevelMap(model);
  if (levels) converted.thinkingLevelMap = levels;
  // Compat describes one transport; regenerate it when the upstream protocol changed.
  const compat = previous?.api === converted.api && previous.compat
    ? previous.compat
    : compatFor(model.id, model.family, converted.api);
  if (compat) converted.compat = compat;
  return converted;
}

const response = await fetch(MODELS_URL, {
  headers: { authorization: `Bearer ${key}`, 'x-opencode-session': `catalog-sync-${Date.now()}` },
});
if (!response.ok) {
  throw new Error(`OpenCode answered ${response.status} ${response.statusText}: ${(await response.text()).slice(0, 200)}`);
}
const payload = await response.json();
if (!Array.isArray(payload.data) || payload.data.length === 0) {
  throw new Error('OpenCode returned no models');
}

const catalog = JSON.parse(fs.readFileSync(CATALOG, 'utf8'));
const index = catalog.providers.findIndex(provider => provider.id === PROVIDER_ID);
const previous = index === -1 ? null : catalog.providers[index];
const previousModels = new Map((previous?.models || []).map(model => [model.id, model]));
const models = payload.data.map(model => convert(model, previousModels.get(model.id)));
const diff = models.filter(model => {
  const before = previousModels.get(model.id);
  return !before || JSON.stringify(before) !== JSON.stringify(model);
});
const entry = { id: PROVIDER_ID, name: 'OpenCode Go', baseUrl: '', models };
if (index === -1) {
  const zen = catalog.providers.findIndex(provider => provider.id === 'opencode');
  catalog.providers.splice(zen === -1 ? catalog.providers.length : zen, 0, entry);
} else {
  catalog.providers[index] = entry;
}
fs.writeFileSync(CATALOG, `${JSON.stringify(catalog, null, 2)}\n`, 'utf8');
console.log(`${index === -1 ? 'Added' : 'Updated'} ${PROVIDER_ID}: ${models.length} models (${diff.length} changed)`);
