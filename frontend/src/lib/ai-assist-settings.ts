// Settings › AI Assist (WORK-581). The backend owns these settings
// (src-tauri/src/live_assist/settings.rs, store `live_assist.json`): DO NOT
// rename a field here alone; `keys_are_camel_case_for_the_frontend` checks.

export type CardKindFlags = { answer: boolean; ask: boolean; fact: boolean; todo: boolean };

export interface ModelOverride {
  provider: string;
  model: string;
}

export interface KnowledgeItem {
  id: string;
  name: string;
  text: string;
  enabled: boolean;
}

export type McpTransport = 'stdio' | 'http' | 'sse';

export interface McpServer {
  id: string;
  name: string;
  enabled: boolean;
  transport: McpTransport;
  command: string;
  args: string[];
  env: Record<string, string>;
  url: string;
  headers: Record<string, string>;
  allowedTools: string[];
}

export interface AssistSettings {
  model: ModelOverride | null;
  aboutMe: string;
  knowledge: KnowledgeItem[];
  instructions: string;
  cardKinds: CardKindFlags;
  research: { enabled: boolean; servers: McpServer[] };
  /** Knowledge and research that ship with the app (settings.rs `Builtin`). */
  builtin: { deepgramOverview: boolean; deepgramDocs: boolean; pipecatDailyDocs: boolean };
}

export interface PromptPreview {
  built_in: string;
  full: string;
  research: string | null;
}

export interface EarlierMeeting {
  id: string;
  title: string;
  created_at: string;
  summary: string;
}

export interface McpToolInfo {
  name: string;
  description: string;
}

/** Same limits as settings.rs. */
export const MAX_CONTEXT_CHARS = 120_000;
export const MAX_INSTRUCTIONS_CHARS = 4_000;

/** File types read as text for Knowledge. */
export const KNOWLEDGE_FILE_ACCEPT = '.txt,.md,.markdown,.csv,.tsv,.json,.yaml,.yml,.html,.xml';

export const DEFAULT_SETTINGS: AssistSettings = {
  model: null,
  aboutMe: '',
  knowledge: [],
  instructions: '',
  cardKinds: { answer: true, ask: true, fact: true, todo: true },
  research: { enabled: true, servers: [] },
  builtin: { deepgramOverview: true, deepgramDocs: true, pipecatDailyDocs: true },
};

/** Same as settings.rs BUILTIN_DOCS_NAME: user servers cannot take it. */
export const BUILTIN_DOCS_NAME = 'Deepgram docs';

/**
 * Characters of standing context the backend counts against
 * MAX_CONTEXT_CHARS (settings.rs `context_chars`, brief aside): about me and
 * enabled knowledge, plus the built-in overview (`builtinChars`, its length)
 * when it is on and fits; when it does not fit the backend leaves it out.
 */
export function contextChars(s: AssistSettings, builtinChars = 0): number {
  const own =
    [...s.aboutMe].length + s.knowledge.filter((k) => k.enabled).reduce((n, k) => n + [...k.text].length, 0);
  const overview = s.builtin.deepgramOverview && own + builtinChars <= MAX_CONTEXT_CHARS ? builtinChars : 0;
  return own + overview;
}

export function newId(prefix: string): string {
  return `${prefix}-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

/**
 * "KEY=value" lines to a map and back (stdio env, HTTP headers). Lines
 * without the separator are dropped; the first separator splits.
 */
export function parsePairs(text: string, sep: '=' | ':'): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split('\n')) {
    const i = line.indexOf(sep);
    if (i <= 0) continue;
    const key = line.slice(0, i).trim();
    if (key) out[key] = line.slice(i + 1).trim();
  }
  return out;
}

export function formatPairs(pairs: Record<string, string>, sep: '=' | ':'): string {
  return Object.entries(pairs)
    .map(([k, v]) => (sep === ':' ? `${k}: ${v}` : `${k}=${v}`))
    .join('\n');
}

/**
 * A command line to program + arguments: whitespace splits, single or double
 * quotes group, no other shell syntax (the command is not run through a shell).
 */
export function splitCommand(line: string): string[] {
  const out: string[] = [];
  let cur = '';
  let quote: '"' | "'" | null = null;
  let has = false;
  for (const ch of line) {
    if (quote) {
      if (ch === quote) quote = null;
      else cur += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch;
      has = true;
    } else if (/\s/.test(ch)) {
      if (has || cur) out.push(cur);
      cur = '';
      has = false;
    } else {
      cur += ch;
    }
  }
  if (has || cur) out.push(cur);
  return out;
}

export function joinCommand(command: string, args: string[]): string {
  const q = (s: string) => (s === '' || /[\s"']/.test(s) ? (s.includes("'") ? `"${s}"` : `'${s}'`) : s);
  return [command, ...args].filter((_, i) => i > 0 || command).map(q).join(' ');
}

/** The DeepHive MCP server on Jake's machines (mcp-connector stdio relay). */
export function deepHiveServer(home: string): McpServer {
  return {
    id: newId('mcp'),
    name: 'DeepHive',
    enabled: true,
    transport: 'stdio',
    command: `${home.replace(/\/$/, '')}/.claude/skills/happier/scripts/mcp-connector`,
    args: ['serve', 'deephive'],
    env: {},
    url: '',
    headers: {},
    allowedTools: ['search_knowledge'],
  };
}

/** Text appended to the brief for one earlier meeting. */
export function briefEntry(m: EarlierMeeting): string {
  const date = m.created_at.slice(0, 10);
  return `Earlier meeting "${m.title}" (${date}):\n${m.summary || '(no summary yet)'}`;
}
