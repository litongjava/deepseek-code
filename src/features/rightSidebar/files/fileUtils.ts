/** Files above this size are not fetched for preview; reads are capped at the same size. */
export const MAX_PREVIEW_BYTES = 1024 * 1024;
/** Above this size the preview skips syntax highlighting and renders plain text. */
export const MAX_HIGHLIGHT_CHARS = 200 * 1024;

/** Last path segment of a folder path; accepts both separators. */
export function baseName(path: string): string {
  return path.split(/[/\\]/).filter(Boolean).pop() || path;
}

/** `path` relative to `root`, or `path` itself when it is outside `root`. */
export function relativePath(root: string, path: string): string {
  const trimmedRoot = root.replace(/[/\\]+$/, '');
  if (trimmedRoot && path.startsWith(trimmedRoot) && /[/\\]/.test(path.charAt(trimmedRoot.length))) {
    return path.slice(trimmedRoot.length + 1);
  }
  return path;
}

/** NUL characters do not occur in text files; the Bridge preview also flags non-UTF-8 data. */
export function looksBinary(content: string): boolean {
  return content.slice(0, 8192).includes('\0');
}

/** Localized size such as "1.5 MB" using the decimal units file managers show. */
export function formatBytes(size: number, locale: string): string {
  const units = ['byte', 'kilobyte', 'megabyte', 'gigabyte'] as const;
  let value = Math.max(0, size);
  let unit = 0;
  while (value >= 1000 && unit < units.length - 1) {
    value /= 1000;
    unit += 1;
  }
  try {
    return new Intl.NumberFormat(locale, { style: 'unit', unit: units[unit], unitDisplay: 'short', maximumFractionDigits: unit === 0 ? 0 : 1 }).format(value);
  } catch {
    return `${value.toFixed(unit === 0 ? 0 : 1)} ${['B', 'KB', 'MB', 'GB'][unit]}`;
  }
}

const LANGUAGE_BY_EXTENSION: Record<string, string> = {
  ts: 'typescript', tsx: 'tsx', js: 'javascript', jsx: 'jsx', mjs: 'javascript', cjs: 'javascript',
  json: 'json', md: 'markdown', markdown: 'markdown', rs: 'rust', py: 'python', go: 'go', java: 'java',
  kt: 'kotlin', c: 'c', h: 'c', cpp: 'cpp', cc: 'cpp', hpp: 'cpp', cs: 'csharp', rb: 'ruby', php: 'php',
  swift: 'swift', sh: 'bash', bash: 'bash', zsh: 'bash', ps1: 'powershell', sql: 'sql', css: 'css',
  scss: 'scss', less: 'less', html: 'markup', htm: 'markup', xml: 'markup', svg: 'markup', vue: 'markup',
  yml: 'yaml', yaml: 'yaml', toml: 'toml', ini: 'ini', dockerfile: 'docker', graphql: 'graphql',
};

/** Prism language for a file name, or null to show plain text. */
export function languageForFile(name: string): string | null {
  const lower = name.toLowerCase();
  if (lower === 'dockerfile') return 'docker';
  const dot = lower.lastIndexOf('.');
  if (dot < 0) return null;
  return LANGUAGE_BY_EXTENSION[lower.slice(dot + 1)] ?? null;
}
