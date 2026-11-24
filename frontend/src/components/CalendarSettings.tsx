'use client';

import { useCallback, useEffect, useMemo, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { homeDir } from '@tauri-apps/api/path';
import { toast } from 'sonner';
import { CalendarClock, ClipboardList, Filter, Plug, RefreshCw, Video } from 'lucide-react';
import { Button } from './ui/button';
import { Input } from './ui/input';
import { Switch } from './ui/switch';
import { Textarea } from './ui/textarea';
import { formatPairs, joinCommand, parsePairs, splitCommand, type McpServer } from '@/lib/ai-assist-settings';
import {
  CALENDAR_BRIEF_PREPARED_EVENT,
  DEFAULT_CALENDAR_SETTINGS,
  blankServer,
  connectorServer,
  eventTime,
  splitKeywords,
  type CalendarSettings as Settings,
  type PreparedBrief,
  type UpcomingEvent,
  type UpcomingList,
} from '@/lib/calendar';

const section = 'bg-white rounded-lg border border-gray-200 p-6 shadow-sm';
const heading = 'flex items-center gap-2 text-lg font-semibold text-gray-900 mb-1';
const help = 'text-sm text-gray-600 mb-4';

/**
 * Settings › Calendar (WORK-677): record calendar meetings by themselves and
 * have each one's brief written beforehand. Stored by the backend
 * (`calendar_save_settings`, store `calendar.json`); the scheduler picks a
 * save up on its next tick.
 */
export function CalendarSettings() {
  const [saved, setSaved] = useState<Settings | null>(null);
  const [form, setForm] = useState<Settings>(DEFAULT_CALENDAR_SETTINGS);
  const [saving, setSaving] = useState(false);
  const [revision, setRevision] = useState(0);

  useEffect(() => {
    invoke<Settings>('calendar_get_settings')
      .then((s) => {
        setSaved(s);
        setForm(s);
      })
      .catch((e) => {
        toast.error('Could not load the calendar settings', { description: String(e) });
        setSaved(DEFAULT_CALENDAR_SETTINGS);
      });
  }, []);

  const dirty = useMemo(() => saved !== null && JSON.stringify(saved) !== JSON.stringify(form), [saved, form]);
  const update = useCallback((patch: Partial<Settings>) => setForm((f) => ({ ...f, ...patch })), []);
  const rules = (patch: Partial<Settings['rules']>) => update({ rules: { ...form.rules, ...patch } });

  const save = async () => {
    setSaving(true);
    try {
      await invoke('calendar_save_settings', { settings: form });
      setSaved(form);
      toast.success('Calendar settings saved');
    } catch (e) {
      toast.error('Could not save the calendar settings', { description: String(e) });
    } finally {
      setSaving(false);
    }
  };

  const useGoogle = async () => {
    update({ server: connectorServer(await homeDir(), 'Google Calendar', 'google_calendar', ['list_events']) });
    setRevision((r) => r + 1);
  };
  const useDeepHive = async () => {
    update({
      lookup: { server: connectorServer(await homeDir(), 'DeepHive', 'deephive', ['search_knowledge']), tool: 'search_knowledge' },
    });
    setRevision((r) => r + 1);
  };

  return (
    <div className="space-y-6 pb-24" data-testid="calendar-settings">
      <div className={section}>
        <h3 className={heading}>
          <Plug className="h-5 w-5 text-gray-600" /> Calendar
        </h3>
        <p className={help}>
          The MCP server Meetily reads your calendar from. It must offer <code>list_events</code> (Google
          Calendar&apos;s MCP server does). Meetily reads the next day every 5 minutes while it is open.
        </p>
        {form.server ? (
          <ServerFields
            key={`cal-${revision}`}
            server={form.server}
            onChange={(p) => update({ server: { ...form.server!, ...p } })}
            onRemove={() => update({ server: null, autoRecord: false, autoPrep: false })}
          />
        ) : (
          <div className="flex gap-2">
            <Button variant="outline" size="sm" onClick={useGoogle}>
              Google Calendar (mcp-connector)
            </Button>
            <Button variant="outline" size="sm" onClick={() => update({ server: blankServer('Calendar') })}>
              Another server
            </Button>
          </div>
        )}
        <label className="mt-3 flex items-center gap-3 text-sm text-gray-700">
          <span className="w-28">Calendar id</span>
          <Input
            aria-label="Calendar id"
            className="h-8 max-w-sm"
            placeholder="primary"
            value={form.calendarId}
            onChange={(e) => update({ calendarId: e.target.value })}
          />
        </label>
      </div>

      <div className={section}>
        <h3 className={heading}>
          <Video className="h-5 w-5 text-gray-600" /> Record meetings by themselves
        </h3>
        <p className={help}>
          When a meeting below qualifies, Meetily starts recording at its start time and shows a notification
          with a Stop button. It stops a recording it started once the meeting is over and no one has spoken for
          a few minutes (or after 15 quiet minutes before the end). A recording you start yourself is never
          stopped by the calendar.
        </p>
        <div className="flex items-center gap-3">
          <Switch
            checked={form.autoRecord}
            aria-label="Record calendar meetings"
            disabled={!form.server}
            onCheckedChange={(v) => update({ autoRecord: v })}
          />
          <span className="text-sm text-gray-900">Record qualifying meetings</span>
        </div>
        <label className="mt-3 flex items-center gap-3 text-sm text-gray-700">
          <span>Stop after</span>
          <Input
            aria-label="Minutes of silence before stopping"
            type="number"
            min={1}
            max={30}
            className="h-8 w-20"
            value={form.stopAfterSilenceMinutes}
            onChange={(e) => update({ stopAfterSilenceMinutes: Number(e.target.value) })}
          />
          <span>minutes of silence once the meeting is over</span>
        </label>
      </div>

      <div className={section}>
        <h3 className={heading}>
          <Filter className="h-5 w-5 text-gray-600" /> Which meetings
        </h3>
        <p className={help}>
          All-day events, focus time, out-of-office, events shown as free and meetings you declined never
          qualify.
        </p>
        <div className="grid grid-cols-1 md:grid-cols-3 gap-3">
          <Rule label="You accepted" help="Or you organize it." on={form.rules.requireAccepted} set={(v) => rules({ requireAccepted: v })} />
          <Rule label="Someone else is invited" help="Rooms do not count." on={form.rules.requireOthers} set={(v) => rules({ requireOthers: v })} />
          <Rule label="It has a video-call link" help="Meet, Zoom, Teams, Webex…" on={form.rules.requireVideo} set={(v) => rules({ requireVideo: v })} />
        </div>
        <div className="mt-4 space-y-3 text-sm text-gray-700">
          <label className="flex items-center gap-3">
            <span className="w-56">At most this many people invited</span>
            <Input
              aria-label="Most people invited"
              type="number"
              min={0}
              className="h-8 w-20"
              value={form.rules.maxAttendees}
              onChange={(e) => rules({ maxAttendees: Math.max(0, Number(e.target.value)) })}
            />
            <span className="text-xs text-gray-500">0 = no limit. Keeps all-hands and webinars out.</span>
          </label>
          <KeywordField
            key={`inc-${revision}`}
            label="Always include titles with"
            hint="Even with no one else on the invite (interview schedulers invite only you)."
            value={form.rules.includeKeywords}
            onChange={(v) => rules({ includeKeywords: v })}
          />
          <KeywordField
            key={`exc-${revision}`}
            label="Never record titles with"
            hint="Comma-separated, any case."
            value={form.rules.excludeKeywords}
            onChange={(v) => rules({ excludeKeywords: v })}
          />
        </div>
      </div>

      <div className={section}>
        <h3 className={heading}>
          <ClipboardList className="h-5 w-5 text-gray-600" /> Prepare the brief
        </h3>
        <p className={help}>
          Before each qualifying meeting, the AI Assist model writes its brief from the calendar event, earlier
          Meetily meetings with the same people or organization, and the lookup below. When a recording starts
          during that meeting, the brief goes into the Meeting brief box, unless you already typed one there:
          yours always wins. Interviews skip the lookup; their prep stays on this computer and with the model.
        </p>
        <div className="flex items-center gap-3">
          <Switch
            checked={form.autoPrep}
            aria-label="Prepare meeting briefs"
            disabled={!form.server}
            onCheckedChange={(v) => update({ autoPrep: v })}
          />
          <span className="text-sm text-gray-900">Prepare a brief for qualifying meetings</span>
        </div>
        <label className="mt-3 flex items-center gap-3 text-sm text-gray-700">
          <Input
            aria-label="Minutes ahead to prepare"
            type="number"
            min={5}
            max={240}
            className="h-8 w-20"
            value={form.prepLeadMinutes}
            onChange={(e) => update({ prepLeadMinutes: Number(e.target.value) })}
          />
          <span>minutes before the meeting</span>
        </label>
        <div className="mt-4">
          <p className="text-sm font-medium text-gray-800 mb-2">Lookup (optional)</p>
          {form.lookup ? (
            <div className="space-y-2">
              <ServerFields
                key={`lookup-${revision}`}
                server={form.lookup.server}
                onChange={(p) => update({ lookup: { ...form.lookup!, server: { ...form.lookup!.server, ...p } } })}
                onRemove={() => update({ lookup: null })}
              />
              <label className="flex items-center gap-3 text-sm text-gray-700">
                <span className="w-28">Tool</span>
                <Input
                  aria-label="Lookup tool"
                  className="h-8 max-w-xs font-mono text-xs"
                  value={form.lookup.tool}
                  onChange={(e) => update({ lookup: { ...form.lookup!, tool: e.target.value } })}
                />
                <span className="text-xs text-gray-500">called with {'{"query": …}'}: the organizations and title</span>
              </label>
            </div>
          ) : (
            <div className="flex gap-2">
              <Button variant="outline" size="sm" onClick={useDeepHive}>
                DeepHive (mcp-connector)
              </Button>
              <Button variant="outline" size="sm" onClick={() => update({ lookup: { server: blankServer('Lookup'), tool: 'search' } })}>
                Another server
              </Button>
            </div>
          )}
        </div>
      </div>

      <Upcoming form={form} saved={saved} dirty={dirty} />

      <div className="fixed bottom-0 left-0 right-0 z-20 border-t border-gray-200 bg-white/95 backdrop-blur">
        <div className="max-w-6xl mx-auto px-8 py-3 flex items-center justify-end gap-3">
          {dirty && <span className="text-sm text-amber-700">Unsaved changes</span>}
          <Button
            variant="outline"
            onClick={() => {
              if (saved) setForm(saved);
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

function Rule({ label, help, on, set }: { label: string; help: string; on: boolean; set: (v: boolean) => void }) {
  return (
    <label className="flex items-start gap-3 rounded border border-gray-200 p-3">
      <Switch checked={on} aria-label={label} onCheckedChange={set} />
      <span>
        <span className="block text-sm font-medium text-gray-900">{label}</span>
        <span className="block text-xs text-gray-500">{help}</span>
      </span>
    </label>
  );
}

function KeywordField({
  label,
  hint,
  value,
  onChange,
}: {
  label: string;
  hint: string;
  value: string[];
  onChange: (v: string[]) => void;
}) {
  // Its own text, so a trailing comma or space survives typing.
  const [text, setText] = useState(value.join(', '));
  return (
    <label className="flex items-center gap-3">
      <span className="w-56">{label}</span>
      <Input
        aria-label={label}
        className="h-8 max-w-sm"
        value={text}
        onChange={(e) => {
          setText(e.target.value);
          onChange(splitKeywords(e.target.value));
        }}
      />
      <span className="text-xs text-gray-500">{hint}</span>
    </label>
  );
}

/** A calendar or lookup server: a local command, or a streamable-HTTP URL. */
function ServerFields({
  server,
  onChange,
  onRemove,
}: {
  server: McpServer;
  onChange: (p: Partial<McpServer>) => void;
  onRemove: () => void;
}) {
  const [commandLine, setCommandLine] = useState(joinCommand(server.command, server.args));
  const [pairs, setPairs] = useState(server.transport === 'http' ? formatPairs(server.headers, ':') : formatPairs(server.env, '='));
  return (
    <div className="rounded border border-gray-200 p-3 space-y-2" data-testid="calendar-server">
      <div className="flex items-center gap-3">
        <Input aria-label="Server name" className="h-8 w-48" value={server.name} onChange={(e) => onChange({ name: e.target.value })} />
        <select
          aria-label="Transport"
          className="h-8 rounded-md border border-gray-300 bg-white px-2 text-sm text-gray-900"
          value={server.transport === 'http' ? 'http' : 'stdio'}
          onChange={(e) => {
            const transport = e.target.value as McpServer['transport'];
            setPairs(transport === 'http' ? formatPairs(server.headers, ':') : formatPairs(server.env, '='));
            onChange({ transport });
          }}
        >
          <option value="stdio">Local command (stdio)</option>
          <option value="http">HTTP (streamable)</option>
        </select>
        <span className="flex-1" />
        <Button variant="outline" size="sm" onClick={onRemove}>
          Remove
        </Button>
      </div>
      {server.transport === 'http' ? (
        <Input
          aria-label="URL"
          className="font-mono text-xs"
          placeholder="https://example.com/mcp"
          value={server.url}
          onChange={(e) => onChange({ url: e.target.value })}
        />
      ) : (
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
      )}
      <Textarea
        aria-label={server.transport === 'http' ? 'Headers' : 'Environment'}
        rows={1}
        className="font-mono text-xs"
        placeholder={server.transport === 'http' ? 'Authorization: Bearer … (one per line)' : 'KEY=value (one per line)'}
        value={pairs}
        onChange={(e) => {
          setPairs(e.target.value);
          if (server.transport === 'http') onChange({ headers: parsePairs(e.target.value, ':') });
          else onChange({ env: parsePairs(e.target.value, '=') });
        }}
      />
    </div>
  );
}

/**
 * The next day's events and what Meetily will do with each. With unsaved
 * changes, "Check" reads the calendar with the form's server and rules (a
 * test); otherwise it shows the scheduler's own last read.
 */
function Upcoming({ form, saved, dirty }: { form: Settings; saved: Settings | null; dirty: boolean }) {
  const [list, setList] = useState<UpcomingList | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [preparing, setPreparing] = useState<string | null>(null);

  const load = useCallback(
    async (refreshNow: boolean) => {
      if (!form.server) {
        setList(null);
        return;
      }
      setLoading(true);
      try {
        const r = await invoke<UpcomingList>('calendar_upcoming', {
          settings: dirty ? form : null,
          refreshNow,
        });
        setList(r);
        setError(null);
      } catch (e) {
        setError(String(e));
      } finally {
        setLoading(false);
      }
    },
    [form, dirty],
  );

  // The saved view follows the scheduler: reload after each read or prep.
  useEffect(() => {
    if (!saved?.server || dirty) return;
    load(false);
    const unlisten = listen(CALENDAR_BRIEF_PREPARED_EVENT, () => load(false));
    return () => {
      unlisten.then((f) => f()).catch(() => {});
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [saved, dirty]);

  const prepareNow = async (ev: UpcomingEvent) => {
    setPreparing(ev.id);
    try {
      await invoke<PreparedBrief>('calendar_prepare_now', { eventId: ev.id });
      toast.success(`Brief prepared for ${ev.title}`);
      await load(false);
    } catch (e) {
      toast.error('Could not prepare the brief', { description: String(e) });
    } finally {
      setPreparing(null);
    }
  };

  const qualifying = list?.events.filter((e) => !e.skipReason).length ?? 0;

  return (
    <div className={section} data-testid="calendar-upcoming">
      <div className="flex items-center justify-between">
        <h3 className={heading}>
          <CalendarClock className="h-5 w-5 text-gray-600" /> Upcoming
        </h3>
        <Button variant="outline" size="sm" onClick={() => load(true)} disabled={loading || !form.server}>
          <RefreshCw className={loading ? 'animate-spin' : ''} /> {dirty ? 'Check with these settings' : 'Read the calendar now'}
        </Button>
      </div>
      <p className={help}>
        {!form.server
          ? 'Set the calendar server to see your meetings.'
          : list
            ? `${list.events.length} event(s) in the next day, ${qualifying} qualifying.` +
              (list.fetchedSecsAgo != null && !dirty ? ` Read ${Math.round(list.fetchedSecsAgo / 60)} min ago.` : '')
            : 'Not read yet.'}
      </p>
      {(error || list?.error) && <p className="text-sm text-red-700 mb-3">Could not read the calendar: {error ?? list?.error}</p>}
      {list && list.events.length > 0 && (
        <ul className="divide-y divide-gray-100">
          {list.events.map((ev) => (
            <li key={ev.id} className="py-2" data-testid="calendar-event">
              <div className="flex items-start gap-3">
                <span
                  className={`mt-1.5 h-2 w-2 shrink-0 rounded-full ${ev.skipReason ? 'bg-gray-300' : 'bg-green-500'}`}
                  title={ev.skipReason ? 'Not recorded' : 'Qualifies'}
                />
                <div className="flex-1 min-w-0">
                  <p className={`text-sm truncate ${ev.skipReason ? 'text-gray-500' : 'text-gray-900 font-medium'}`} title={ev.title}>
                    {ev.title || '(no title)'}
                    {ev.interview && <span className="ml-2 text-xs font-normal text-purple-700">interview</span>}
                  </p>
                  <p className="text-xs text-gray-500">
                    {eventTime(ev.start, ev.end)}
                    {ev.skipReason ? ` · skipped: ${ev.skipReason}` : ' · will record'}
                    {ev.briefState === 'ready' && ' · brief ready'}
                    {ev.briefState === 'preparing' && ' · preparing brief…'}
                    {ev.briefState === 'failed' && ` · brief failed: ${ev.briefError ?? ''}`}
                  </p>
                  {ev.brief && (
                    <details className="mt-1">
                      <summary className="cursor-pointer text-xs text-blue-700">
                        View the brief ({ev.brief.sources.join(', ')})
                      </summary>
                      <pre
                        data-testid="calendar-brief"
                        className="mt-1 max-h-80 overflow-auto whitespace-pre-wrap rounded bg-gray-50 border border-gray-200 p-3 text-xs text-gray-800"
                      >
                        {ev.brief.text}
                      </pre>
                    </details>
                  )}
                </div>
                {!dirty && (
                  <Button
                    variant="outline"
                    size="sm"
                    onClick={() => prepareNow(ev)}
                    disabled={preparing !== null || ev.briefState === 'preparing'}
                  >
                    {preparing === ev.id ? 'Preparing…' : ev.brief ? 'Prepare again' : 'Prepare now'}
                  </Button>
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
