// Calendar: auto-record and meeting prep (WORK-677). Types mirror
// src-tauri/src/calendar (CalendarSettings, Rules, Lookup, PreparedBrief,
// Upcoming, AutoRecordEvent); DO NOT rename a field or event here alone.

import { newId, type McpServer } from './ai-assist-settings';
export { connectorServer } from './ai-assist-settings';

/** Emitted when the calendar starts, stops or skips a recording (calendar::AUTO_RECORD_EVENT). */
export const CALENDAR_AUTO_RECORD_EVENT = 'calendar-auto-record';
/** Emitted after a brief was prepared, or the calendar was re-read (calendar::BRIEF_PREPARED_EVENT). */
export const CALENDAR_BRIEF_PREPARED_EVENT = 'calendar-brief-prepared';

export interface CalendarRules {
  requireAccepted: boolean;
  requireOthers: boolean;
  requireVideo: boolean;
  /** 0 = no limit. */
  maxAttendees: number;
  includeKeywords: string[];
  excludeKeywords: string[];
}

export interface CalendarLookup {
  server: McpServer;
  tool: string;
}

export interface CalendarSettings {
  autoRecord: boolean;
  autoPrep: boolean;
  server: McpServer | null;
  calendarId: string;
  rules: CalendarRules;
  prepLeadMinutes: number;
  stopAfterSilenceMinutes: number;
  lookup: CalendarLookup | null;
}

export interface PreparedBrief {
  eventId: string;
  title: string;
  start: string;
  end: string;
  text: string;
  preparedAt: string;
  sources: string[];
}

export interface UpcomingEvent {
  id: string;
  title: string;
  start: string;
  end: string;
  /** null: it qualifies. */
  skipReason: string | null;
  interview: boolean;
  briefState: 'none' | 'preparing' | 'ready' | 'failed';
  briefError: string | null;
  brief: PreparedBrief | null;
  handled: boolean;
}

export interface UpcomingList {
  events: UpcomingEvent[];
  fetchedSecsAgo: number | null;
  error: string | null;
}

export interface AutoRecordEvent {
  state: 'started' | 'stopped' | 'skipped' | 'failed';
  eventId: string;
  title: string;
  message: string;
}

export const DEFAULT_CALENDAR_SETTINGS: CalendarSettings = {
  autoRecord: false,
  autoPrep: false,
  server: null,
  calendarId: '',
  rules: {
    requireAccepted: true,
    requireOthers: true,
    requireVideo: true,
    maxAttendees: 25,
    includeKeywords: ['interview'],
    excludeKeywords: [],
  },
  prepLeadMinutes: 30,
  stopAfterSilenceMinutes: 3,
  lookup: null,
};

/** A blank server for the calendar or lookup field. */
export function blankServer(name: string): McpServer {
  return {
    id: newId('mcp'),
    name,
    enabled: true,
    transport: 'stdio',
    command: '',
    args: [],
    env: {},
    url: '',
    headers: {},
    allowedTools: [],
  };
}

/** "a, b ,, c" -> ["a", "b", "c"]. */
export function splitKeywords(text: string): string[] {
  return text
    .split(',')
    .map((k) => k.trim())
    .filter(Boolean);
}

/** "Thu 19 Nov, 15:00–15:30" in local time. */
export function eventTime(start: string, end: string): string {
  const s = new Date(start);
  const e = new Date(end);
  const day = s.toLocaleDateString(undefined, { weekday: 'short', day: 'numeric', month: 'short' });
  const hm = (d: Date) => d.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit', hour12: false });
  return `${day}, ${hm(s)}–${hm(e)}`;
}
