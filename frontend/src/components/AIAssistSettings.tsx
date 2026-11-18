'use client';

import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { homeDir } from '@tauri-apps/api/path';
import { toast } from 'sonner';
import { BookOpen, Bot, FileText, Plug, Plus, RotateCcw, Trash2, Upload, UserRound, Wrench } from 'lucide-react';
import { Button } from './ui/button';
import { Input } from './ui/input';
import { Switch } from './ui/switch';
import { Textarea } from './ui/textarea';
import { LiveAssistSettings } from './LiveAssistSettings';
import {
  AssistSettings,
  DEFAULT_SETTINGS,
  KNOWLEDGE_FILE_ACCEPT,
  MAX_CONTEXT_CHARS,
  MAX_INSTRUCTIONS_CHARS,
  McpServer,
  McpToolInfo,
  PromptPreview,
  contextChars,
  overviewIncluded,
  deepHiveServer,
  formatPairs,
  joinCommand,
  newId,
  parsePairs,
  splitCommand,
} from '@/lib/ai-assist-settings';
import { KIND_LABEL, type AssistCardKind } from '@/lib/live-assist';
import type { LiveEngine } from '@/lib/live-engine';

const PROVIDERS: { value: string; label: string }[] = [
  { value: 'claude', label: 'Anthropic (Claude)' },
  { value: 'openai', label: 'OpenAI' },
  { value: 'groq', label: 'Groq' },
  { value: 'openrouter', label: 'OpenRouter' },
  { value: 'ollama', label: 'Ollama' },
  { value: 'custom-openai', label: 'Custom OpenAI endpoint' },
];

// Rust owns the model lists (summary/*: FALLBACK_MODELS + the live list);
// the same commands the Summary model picker uses.
const MODEL_COMMANDS: Record<string, string> = {
  openai: 'get_openai_models',
  claude: 'get_anthropic_models',
  groq: 'get_groq_models',
};

const KINDS: AssistCardKind[] = ['answer', 'ask', 'fact', 'todo'];

const KIND_HELP: Record<AssistCardKind, string> = {
  answer: 'A direct reply to a question they asked.',
  ask: 'A follow-up question you could ask.',
  fact: 'A fact to keep in mind, from the call or your knowledge.',
  todo: 'A commitment or follow-up to note down.',
};

const section = 'bg-white rounded-lg border border-gray-200 p-6 shadow-sm';
const heading = 'flex items-center gap-2 text-lg font-semibold text-gray-900 mb-1';
const help = 'text-sm text-gray-600 mb-4';

/**
 * Settings › AI Assist (WORK-581): what the live-assist cards know and how
 * they are made. Stored by the backend (`ai_assist_save_settings`, store
 * `live_assist.json`); a running call picks a save up at once.
 */
export function AIAssistSettings() {
  const [saved, setSaved] = useState<AssistSettings | null>(null);
  const [form, setForm] = useState<AssistSettings>(DEFAULT_SETTINGS);
  const [summaryModel, setSummaryModel] = useState<string>('');
  const [engine, setEngine] = useState<LiveEngine | null>(null);
  const [preview, setPreview] = useState<PromptPreview | null>(null);
  const [saving, setSaving] = useState(false);
  const [builtinText, setBuiltinText] = useState<string | null>(null);
  // The saved meeting brief's length: the backend counts it against the
  // context limit too (settings.rs `context_chars`).
  const [briefChars, setBriefChars] = useState(0);
  // Bumped when the form is replaced wholesale (Discard), so editors that
  // keep their own text (command line, env/header pairs) start over from it.
  const [revision, setRevision] = useState(0);

  useEffect(() => {
    invoke<AssistSettings>('ai_assist_get_settings')
      .then((s) => {
        setSaved(s);
        setForm(s);
      })
      .catch((e) => {
        toast.error('Could not load the AI Assist settings', { description: String(e) });
        setSaved(DEFAULT_SETTINGS);
        setForm(DEFAULT_SETTINGS);
      });
    invoke<{ provider: string; model: string } | null>('api_get_model_config')
      .then((c) => setSummaryModel(c ? `${c.provider} / ${c.model}` : 'not set'))
      .catch(() => setSummaryModel('not set'));
    invoke<LiveEngine>('live_engine_get').then(setEngine).catch(() => setEngine('direct'));
    invoke<string>('ai_assist_builtin_knowledge')
      .then(setBuiltinText)
      .catch(() => setBuiltinText(''));
    invoke<string>('ai_assist_get_brief')
      .then((b) => setBriefChars([...b].length))
      .catch(() => setBriefChars(0));
  }, []);

  // The prompt as it would be with the unsaved form.
  useEffect(() => {
    const t = setTimeout(() => {
      invoke<PromptPreview>('ai_assist_prompt_preview', { settings: form })
        .then(setPreview)
        .catch((e) => console.error('ai assist: prompt preview failed', e));
    }, 300);
    return () => clearTimeout(t);
  }, [form]);

  const dirty = useMemo(() => saved !== null && JSON.stringify(saved) !== JSON.stringify(form), [saved, form]);
  const update = useCallback((patch: Partial<AssistSettings>) => setForm((f) => ({ ...f, ...patch })), []);

  const save = async () => {
    setSaving(true);
    try {
      await invoke('ai_assist_save_settings', { settings: form });
      setSaved(form);
      toast.success('AI Assist settings saved');
    } catch (e) {
      toast.error('Could not save the AI Assist settings', { description: String(e) });
    } finally {
      setSaving(false);
    }
  };

  if (saved === null) {
    return <p className="text-sm text-gray-500 p-6">Loading…</p>;
  }

  const builtinChars = builtinText ? [...builtinText].length : 0;
  const chars = contextChars(form, builtinChars, briefChars);
  const overviewIn = overviewIncluded(form, builtinChars, briefChars);

  return (
    <div className="space-y-6 mt-6 pb-24" data-testid="ai-assist-settings">
      <LiveAssistSettings />

      <ModelSection form={form} update={update} summaryModel={summaryModel} />

      <div className={section}>
        <h3 className={heading}>
          <UserRound className="h-5 w-5 text-gray-600" /> About me
        </h3>
        <p className={help}>
          A standing note on who you are, your company and what you sell. Sent with every card call.
        </p>
        <Textarea
          aria-label="About me"
          rows={4}
          value={form.aboutMe}
          placeholder="I'm an Applied Engineer at Deepgram. We sell speech-to-text, text-to-speech and a voice agent API…"
          onChange={(e) => update({ aboutMe: e.target.value })}
        />
      </div>

      <KnowledgeSection
        form={form}
        update={update}
        chars={chars}
        briefChars={briefChars}
        builtinText={builtinText}
        overviewIn={overviewIn}
      />

      <div className={section}>
        <h3 className={heading}>
          <Wrench className="h-5 w-5 text-gray-600" /> Instructions
        </h3>
        <p className={help}>
          Extra rules added after the built-in ones. The built-in rules always stay: never invent facts, quote the
          transcript, reply in the card format.
        </p>
        <Textarea
          aria-label="Instructions"
          rows={4}
          value={form.instructions}
          placeholder="Prefer questions that qualify volume and latency needs. Use Deepgram product names."
          onChange={(e) => update({ instructions: e.target.value })}
        />
        <div className="flex items-center justify-between mt-2 text-xs text-gray-500">
          <span>
            {[...form.instructions].length.toLocaleString()} / {MAX_INSTRUCTIONS_CHARS.toLocaleString()} characters
          </span>
          <Button variant="outline" size="sm" onClick={() => update({ instructions: '' })} disabled={!form.instructions}>
            <RotateCcw /> Reset to built-in only
          </Button>
        </div>
        <details className="mt-4">
          <summary className="cursor-pointer text-sm font-medium text-gray-700">View the full prompt</summary>
          <p className="text-xs text-gray-500 mt-2">
            What each card call is sent before the transcript, with the meeting brief as it is now. Everything
            before &quot;Me&apos;s extra instructions&quot; and &quot;Me&apos;s background&quot; is built in.
          </p>
          <pre
            data-testid="ai-assist-prompt"
            className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap rounded bg-gray-50 border border-gray-200 p-3 text-xs text-gray-800"
          >
            {preview?.full ?? '…'}
          </pre>
          {preview?.research && (
            <>
              <p className="text-xs text-gray-500 mt-3">Research lookups are sent:</p>
              <pre className="mt-1 max-h-64 overflow-auto whitespace-pre-wrap rounded bg-gray-50 border border-gray-200 p-3 text-xs text-gray-800">
                {preview.research}
              </pre>
            </>
          )}
        </details>
      </div>

      <div className={section}>
        <h3 className={heading}>
          <FileText className="h-5 w-5 text-gray-600" /> Card kinds
        </h3>
        <p className={help}>Which cards to show. At least one stays on.</p>
        <div className="grid grid-cols-2 gap-3">
          {KINDS.map((kind) => {
            const on = form.cardKinds[kind];
            const last = on && KINDS.filter((k) => form.cardKinds[k]).length === 1;
            return (
              <label key={kind} className="flex items-start gap-3 rounded border border-gray-200 p-3">
                <Switch
                  checked={on}
                  disabled={last}
                  aria-label={KIND_LABEL[kind]}
                  onCheckedChange={(v) => update({ cardKinds: { ...form.cardKinds, [kind]: v } })}
                />
                <span>
                  <span className="block text-sm font-medium text-gray-900">{KIND_LABEL[kind]}</span>
                  <span className="block text-xs text-gray-500">{KIND_HELP[kind]}</span>
                </span>
              </label>
            );
          })}
        </div>
      </div>

      <ResearchSection form={form} update={update} engine={engine} revision={revision} />

      <div className="fixed bottom-0 left-0 right-0 z-20 border-t border-gray-200 bg-white/95 backdrop-blur">
        <div className="max-w-6xl mx-auto px-8 py-3 flex items-center justify-end gap-3">
          {dirty && <span className="text-sm text-amber-700">Unsaved changes</span>}
          <Button
            variant="outline"
            onClick={() => {
              setForm(saved);
              setRevision((r) => r + 1);
            }}
            disabled={!dirty || saving}
          >
            Discard
          </Button>
          <Button onClick={save} disabled={!dirty || saving}>
            {saving ? 'Saving…' : 'Save'}
          </Button>
        </div>
      </div>
    </div>
  );
}

type SectionProps = { form: AssistSettings; update: (p: Partial<AssistSettings>) => void };

function ModelSection({ form, update, summaryModel }: SectionProps & { summaryModel: string }) {
  const [models, setModels] = useState<string[]>([]);
  const provider = form.model?.provider ?? 'claude';
  const own = form.model !== null;

  // Per provider, not per keystroke: the list command calls the provider's API.
  useEffect(() => {
    if (!own) return;
    const command = MODEL_COMMANDS[provider];
    if (!command) {
      setModels([]);
      return;
    }
    let current = true;
    invoke<string>('api_get_api_key', { provider })
      .catch(() => '')
      .then((key) => invoke<{ id: string }[]>(command, { apiKey: key?.trim() || null }))
      .then((list) => {
        if (current) setModels(list.map((m) => m.id));
      })
      .catch((e) => console.error('ai assist: model list failed', e));
    return () => {
      current = false;
    };
  }, [own, provider]);

  return (
    <div className={section}>
      <h3 className={heading}>
        <Bot className="h-5 w-5 text-gray-600" /> Model
      </h3>
      <p className={help}>
        Cards use the summary model by default. A faster model just for cards keeps them quick; it uses the API key
        saved for that provider in Settings › Summary.
      </p>
      <div className="space-y-3">
        <label className="flex items-center gap-2 text-sm">
          <input type="radio" name="assist-model" checked={!form.model} onChange={() => update({ model: null })} />
          Same as the summary model <span className="text-gray-500">({summaryModel || '…'})</span>
        </label>
        <label className="flex items-center gap-2 text-sm">
          <input
            type="radio"
            name="assist-model"
            checked={!!form.model}
            onChange={() => update({ model: form.model ?? { provider: 'claude', model: '' } })}
          />
          A model just for AI Assist
        </label>
        {form.model && (
          <div className="grid grid-cols-2 gap-3 pl-6">
            <select
              aria-label="Assist provider"
              className="h-9 rounded-md border border-gray-300 bg-white px-2 text-sm"
              value={provider}
              onChange={(e) => update({ model: { provider: e.target.value, model: '' } })}
            >
              {PROVIDERS.map((p) => (
                <option key={p.value} value={p.value}>
                  {p.label}
                </option>
              ))}
            </select>
            <Input
              aria-label="Assist model"
              list="ai-assist-models"
              placeholder="Model id"
              value={form.model.model}
              onChange={(e) => update({ model: { provider, model: e.target.value } })}
            />
            <datalist id="ai-assist-models">
              {models.map((m) => (
                <option key={m} value={m} />
              ))}
            </datalist>
          </div>
        )}
      </div>
    </div>
  );
}

function KnowledgeSection({
  form,
  update,
  chars,
  briefChars,
  builtinText,
  overviewIn,
}: SectionProps & { chars: number; briefChars: number; builtinText: string | null; overviewIn: boolean }) {
  const fileRef = useRef<HTMLInputElement>(null);
  const [draftName, setDraftName] = useState('');
  const [draftText, setDraftText] = useState('');

  const addText = () => {
    if (!draftText.trim()) return;
    update({
      knowledge: [
        ...form.knowledge,
        { id: newId('kn'), name: draftName.trim() || `Note ${form.knowledge.length + 1}`, text: draftText, enabled: true },
      ],
    });
    setDraftName('');
    setDraftText('');
  };

  const addFiles = async (files: FileList | null) => {
    if (!files?.length) return;
    const added = [];
    for (const file of Array.from(files)) {
      try {
        added.push({ id: newId('kn'), name: file.name, text: await file.text(), enabled: true });
      } catch (e) {
        toast.error(`Could not read ${file.name}`, { description: String(e) });
      }
    }
    update({ knowledge: [...form.knowledge, ...added] });
    if (fileRef.current) fileRef.current.value = '';
  };

  const over = chars > MAX_CONTEXT_CHARS;

  return (
    <div className={section}>
      <h3 className={heading}>
        <BookOpen className="h-5 w-5 text-gray-600" /> Knowledge
      </h3>
      <p className={help}>
        Product facts, pricing, FAQs, objection handling. Sent as cached context on Claude, so calls do not pay for it
        in full each time. Text files only (.txt, .md, .csv, .json…); for a PDF, paste its text.
      </p>
      <BuiltinOverview
        text={builtinText}
        on={form.builtin.deepgramOverview}
        included={overviewIn}
        onChange={(v) => update({ builtin: { ...form.builtin, deepgramOverview: v } })}
      />
      {form.knowledge.length > 0 && (
        <ul className="space-y-2 mb-4">
          {form.knowledge.map((k, i) => (
            <li key={k.id} className="rounded border border-gray-200 p-3">
              <div className="flex items-center gap-3">
                <Switch
                  checked={k.enabled}
                  aria-label={`Use ${k.name}`}
                  onCheckedChange={(v) =>
                    update({ knowledge: form.knowledge.map((x, j) => (j === i ? { ...x, enabled: v } : x)) })
                  }
                />
                <Input
                  aria-label="Knowledge name"
                  className="h-8 flex-1"
                  value={k.name}
                  onChange={(e) =>
                    update({ knowledge: form.knowledge.map((x, j) => (j === i ? { ...x, name: e.target.value } : x)) })
                  }
                />
                <span className="text-xs text-gray-500 w-28 text-right">{[...k.text].length.toLocaleString()} chars</span>
                <button
                  title="Remove"
                  className="text-gray-400 hover:text-red-600"
                  onClick={() => update({ knowledge: form.knowledge.filter((_, j) => j !== i) })}
                >
                  <Trash2 className="w-4 h-4" />
                </button>
              </div>
              <details className="mt-2">
                <summary className="cursor-pointer text-xs text-gray-500">Edit text</summary>
                <Textarea
                  aria-label={`${k.name} text`}
                  rows={6}
                  className="mt-2 text-xs"
                  value={k.text}
                  onChange={(e) =>
                    update({ knowledge: form.knowledge.map((x, j) => (j === i ? { ...x, text: e.target.value } : x)) })
                  }
                />
              </details>
            </li>
          ))}
        </ul>
      )}
      <div className="rounded border border-dashed border-gray-300 p-3 space-y-2">
        <Input aria-label="New knowledge name" placeholder="Name (e.g. Pricing 2026)" value={draftName} onChange={(e) => setDraftName(e.target.value)} />
        <Textarea
          aria-label="New knowledge text"
          rows={4}
          placeholder="Paste text…"
          value={draftText}
          onChange={(e) => setDraftText(e.target.value)}
        />
        <div className="flex gap-2">
          <Button variant="outline" size="sm" onClick={addText} disabled={!draftText.trim()}>
            <Plus /> Add text
          </Button>
          <Button variant="outline" size="sm" onClick={() => fileRef.current?.click()}>
            <Upload /> Add files
          </Button>
          <input
            ref={fileRef}
            type="file"
            multiple
            accept={KNOWLEDGE_FILE_ACCEPT}
            className="hidden"
            onChange={(e) => addFiles(e.target.files)}
          />
        </div>
      </div>
      <p className={`text-xs mt-2 ${over ? 'text-red-600' : 'text-gray-500'}`}>
        {briefChars > 0 ? 'Built-in overview, about me, enabled knowledge and the meeting brief' : 'Built-in overview, about me and enabled knowledge'}
        : {chars.toLocaleString()} / {MAX_CONTEXT_CHARS.toLocaleString()} characters
        {over && ' — over the limit; turn some off to save'}
      </p>
    </div>
  );
}

function ResearchSection({
  form,
  update,
  engine,
  revision,
}: SectionProps & { engine: LiveEngine | null; revision: number }) {
  const servers = form.research.servers;
  const setServers = (next: McpServer[]) => update({ research: { ...form.research, servers: next } });
  const setServer = (i: number, patch: Partial<McpServer>) =>
    setServers(servers.map((s, j) => (j === i ? { ...s, ...patch } : s)));

  const addDeepHive = async () => {
    try {
      setServers([...servers, deepHiveServer(await homeDir())]);
    } catch (e) {
      toast.error('Could not find your home folder', { description: String(e) });
    }
  };

  return (
    <div className={section}>
      <div className="flex items-start justify-between">
        <div className="flex-1">
          <h3 className={heading}>
            <Plug className="h-5 w-5 text-gray-600" /> Research with MCP tools
          </h3>
          <p className={help}>
            When the other side asks for a fact your context does not hold, a separate lookup runs with these MCP
            servers&apos; tools and adds a follow-up card a few seconds later. The fast cards never wait for it. Pipecat
            live engine only (Settings › Transcription).
          </p>
          {form.research.enabled && !form.cardKinds.answer && !form.cardKinds.fact && (
            <p className="text-xs text-amber-700 mb-3">
              Lookups answer as Answer or Fact cards, so research stays off while both are turned off.
            </p>
          )}
          {engine === 'direct' && (
            <p className="text-xs text-amber-700 mb-3">
              The live engine is Direct, so research does not run until it is switched to Pipecat.
            </p>
          )}
        </div>
        <Switch
          className="ml-6"
          aria-label="Research with MCP tools"
          checked={form.research.enabled}
          onCheckedChange={(v) => update({ research: { ...form.research, enabled: v } })}
        />
      </div>
      <BuiltinDocs
        on={form.builtin.deepgramDocs}
        onChange={(v) => update({ builtin: { ...form.builtin, deepgramDocs: v } })}
      />
      <BuiltinDocs
        variant="pipecatDaily"
        on={form.builtin.pipecatDailyDocs}
        onChange={(v) => update({ builtin: { ...form.builtin, pipecatDailyDocs: v } })}
      />
      <ul className="space-y-3">
        {servers.map((s, i) => (
          <McpServerEditor
            key={`${s.id}-${revision}`}
            server={s}
            onChange={(patch) => setServer(i, patch)}
            onRemove={() => setServers(servers.filter((_, j) => j !== i))}
          />
        ))}
      </ul>
      <div className="flex gap-2 mt-3">
        <Button
          variant="outline"
          size="sm"
          onClick={() =>
            setServers([
              ...servers,
              {
                id: newId('mcp'),
                name: `Server ${servers.length + 1}`,
                enabled: true,
                transport: 'stdio',
                command: '',
                args: [],
                env: {},
                url: '',
                headers: {},
                allowedTools: [],
              },
            ])
          }
        >
          <Plus /> Add server
        </Button>
        {!servers.some((s) => s.name.toLowerCase() === 'deephive') && (
          <Button variant="outline" size="sm" onClick={addDeepHive}>
            <Plus /> Add DeepHive
          </Button>
        )}
      </div>
      <p className="text-xs text-gray-500 mt-3">
        Headers and environment values are stored in plain text in Meetily&apos;s settings, like the summary API keys.
        claude.ai connectors cannot be reached from an API key; add the server itself.
      </p>
    </div>
  );
}

function McpServerEditor({
  server,
  onChange,
  onRemove,
}: {
  server: McpServer;
  onChange: (p: Partial<McpServer>) => void;
  onRemove: () => void;
}) {
  const [commandLine, setCommandLine] = useState(joinCommand(server.command, server.args));
  const [pairs, setPairs] = useState(
    server.transport === 'stdio' ? formatPairs(server.env, '=') : formatPairs(server.headers, ':'),
  );
  const [tools, setTools] = useState<McpToolInfo[] | null>(null);
  const [testing, setTesting] = useState(false);

  const test = async () => {
    setTesting(true);
    try {
      const list = await invoke<McpToolInfo[]>('ai_assist_test_mcp', { server });
      setTools(list);
      toast.success(`${server.name}: ${list.length} tool(s)`);
    } catch (e) {
      setTools(null);
      toast.error(`${server.name} did not connect`, { description: String(e) });
    } finally {
      setTesting(false);
    }
  };

  // An empty allowlist means every tool the server lists, so unchecking
  // the last allowed tool must not empty it: that would grant everything.
  // The last box stays checked; "Allow all tools" is the explicit way to [].
  const toggleTool = (name: string, on: boolean) => {
    const current = new Set(server.allowedTools);
    if (on) current.add(name);
    else if (current.size > 1) current.delete(name);
    onChange({ allowedTools: [...current] });
  };

  return (
    <li className="rounded border border-gray-200 p-3 space-y-2" data-testid="mcp-server">
      <div className="flex items-center gap-3">
        <Switch checked={server.enabled} aria-label={`Use ${server.name}`} onCheckedChange={(v) => onChange({ enabled: v })} />
        <Input aria-label="Server name" className="h-8 w-48" value={server.name} onChange={(e) => onChange({ name: e.target.value })} />
        <select
          aria-label="Transport"
          className="h-8 rounded-md border border-gray-300 bg-white px-2 text-sm"
          value={server.transport}
          onChange={(e) => {
            const transport = e.target.value as McpServer['transport'];
            setPairs(transport === 'stdio' ? formatPairs(server.env, '=') : formatPairs(server.headers, ':'));
            onChange({ transport });
          }}
        >
          <option value="stdio">Local command (stdio)</option>
          <option value="http">HTTP (streamable)</option>
          <option value="sse">HTTP (SSE)</option>
        </select>
        <span className="flex-1" />
        <Button variant="outline" size="sm" onClick={test} disabled={testing}>
          {testing ? 'Testing…' : 'Test'}
        </Button>
        <button title="Remove" className="text-gray-400 hover:text-red-600" onClick={onRemove}>
          <Trash2 className="w-4 h-4" />
        </button>
      </div>
      {server.transport === 'stdio' ? (
        <>
          <Input
            aria-label="Command"
            className="font-mono text-xs"
            placeholder="/path/to/server --flag value"
            value={commandLine}
            onChange={(e) => {
              setCommandLine(e.target.value);
              const [command = '', ...args] = splitCommand(e.target.value);
              onChange({ command, args });
            }}
          />
          <Textarea
            aria-label="Environment"
            rows={2}
            className="font-mono text-xs"
            placeholder="KEY=value (one per line)"
            value={pairs}
            onChange={(e) => {
              setPairs(e.target.value);
              onChange({ env: parsePairs(e.target.value, '=') });
            }}
          />
        </>
      ) : (
        <>
          <Input
            aria-label="URL"
            className="font-mono text-xs"
            placeholder="https://example.com/mcp"
            value={server.url}
            onChange={(e) => onChange({ url: e.target.value })}
          />
          <Textarea
            aria-label="Headers"
            rows={2}
            className="font-mono text-xs"
            placeholder="Authorization: Bearer … (one per line)"
            value={pairs}
            onChange={(e) => {
              setPairs(e.target.value);
              onChange({ headers: parsePairs(e.target.value, ':') });
            }}
          />
        </>
      )}
      <div className="text-xs text-gray-600">
        <span className="font-medium">Allowed tools:</span>{' '}
        {server.allowedTools.length ? server.allowedTools.join(', ') : 'all tools the server lists'}
        {server.allowedTools.length > 0 && (
          <button className="ml-2 text-blue-600 hover:underline" onClick={() => onChange({ allowedTools: [] })}>
            Allow all tools
          </button>
        )}
        {tools && tools.length > 0 && (
          <div className="mt-2 grid grid-cols-2 gap-1">
            {tools.map((t) => (
              <label key={t.name} className="flex items-center gap-2" title={t.description}>
                <input
                  type="checkbox"
                  checked={server.allowedTools.includes(t.name)}
                  disabled={server.allowedTools.length === 1 && server.allowedTools[0] === t.name}
                  title={
                    server.allowedTools.length === 1 && server.allowedTools[0] === t.name
                      ? 'At least one tool stays allowed; turn the server off to use none'
                      : undefined
                  }
                  onChange={(e) => toggleTool(t.name, e.target.checked)}
                />
                <span className="font-mono">{t.name}</span>
              </label>
            ))}
          </div>
        )}
        {!tools && <span className="text-gray-400"> · Test to pick from the server&apos;s tools</span>}
      </div>
    </li>
  );
}

/** The built-in Deepgram overview (deepgram.com/llms.txt), on by default. */
function BuiltinOverview({
  text,
  on,
  included,
  onChange,
}: {
  text: string | null;
  on: boolean;
  included: boolean;
  onChange: (v: boolean) => void;
}) {
  return (
    <div className="rounded border border-blue-200 bg-blue-50/40 p-3 mb-4" data-testid="builtin-overview">
      <div className="flex items-center gap-3">
        <Switch checked={on} aria-label="Use the built-in Deepgram overview" onCheckedChange={onChange} />
        <span className="flex-1 text-sm font-medium text-gray-900">
          Deepgram, Pipecat and Daily public overview (built in)
        </span>
        <span className="text-xs text-gray-500">{text ? `${[...text].length.toLocaleString()} chars` : ''}</span>
      </div>
      <p className="text-xs text-gray-600 mt-1 ml-12">
        Deepgram&apos;s products, models, pricing tiers, deployment and trust facts (deepgram.com/llms.txt), plus what
        Pipecat and Daily are and do (their docs&apos; llms.txt). Ships with the app, so a colleague gets aware cards
        with nothing to paste.
      </p>
      {on && text && !included && (
        <p className="text-xs text-amber-700 mt-1 ml-12" data-testid="builtin-overview-left-out">
          Left out for now: it does not fit with your own context and the meeting brief. Turn some knowledge off to bring
          it back.
        </p>
      )}
      {text && (
        <details className="mt-2 ml-12">
          <summary className="cursor-pointer text-xs text-gray-500">View text</summary>
          <pre className="mt-2 max-h-64 overflow-auto whitespace-pre-wrap rounded bg-white border border-gray-200 p-2 text-xs text-gray-800">
            {text}
          </pre>
        </details>
      )}
    </div>
  );
}

const BUILTIN_DOCS = {
  deepgram: {
    title: 'Deepgram docs (built in)',
    command: 'ai_assist_test_builtin_docs',
    help: "Deepgram's hosted docs search (developers.deepgram.com), signed in with the Deepgram API key saved for transcription. Lookups answer from the official docs.",
  },
  pipecatDaily: {
    title: 'Pipecat and Daily docs (built in)',
    command: 'ai_assist_test_builtin_pipecat_daily',
    help: "Pipecat's and Daily's public docs search (docs.pipecat.ai, docs.daily.co). No sign-in; search only.",
  },
} as const;

/** A built-in docs research server, on by default. */
function BuiltinDocs({
  on,
  onChange,
  variant = 'deepgram',
}: {
  on: boolean;
  onChange: (v: boolean) => void;
  variant?: keyof typeof BUILTIN_DOCS;
}) {
  const docs = BUILTIN_DOCS[variant];
  const [testing, setTesting] = useState(false);
  const test = async () => {
    setTesting(true);
    try {
      const tools = await invoke<McpToolInfo[]>(docs.command);
      toast.success(`${docs.title}: ${tools.map((t) => t.name).join(', ') || 'no tools'}`);
    } catch (e) {
      toast.error(`${docs.title}: test failed`, { description: String(e) });
    } finally {
      setTesting(false);
    }
  };
  return (
    <div className="rounded border border-blue-200 bg-blue-50/40 p-3 mb-3" data-testid={`builtin-docs-${variant}`}>
      <div className="flex items-center gap-3">
        <Switch checked={on} aria-label={`Use the ${docs.title}`} onCheckedChange={onChange} />
        <span className="flex-1 text-sm font-medium text-gray-900">{docs.title}</span>
        <Button variant="outline" size="sm" onClick={test} disabled={testing}>
          {testing ? 'Testing…' : 'Test'}
        </Button>
      </div>
      <p className="text-xs text-gray-600 mt-1 ml-12">{docs.help}</p>
    </div>
  );
}
