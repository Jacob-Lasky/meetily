import { describe, expect, test } from 'bun:test';
import { DEFAULT_CALENDAR_SETTINGS, connectorServer, splitKeywords } from '../../src/lib/calendar';

describe('calendar settings helpers', () => {
  test('keywords split on commas, trimmed, blanks dropped', () => {
    expect(splitKeywords(' interview, Candidate ,, ')).toEqual(['interview', 'Candidate']);
    expect(splitKeywords('')).toEqual([]);
  });

  test('the Google Calendar quick-fill runs the mcp-connector relay', () => {
    const s = connectorServer('/home/u/', 'Google Calendar', 'google_calendar', ['list_events']);
    expect(s.command).toBe('/home/u/.claude/skills/mcp-connector/scripts/mcp-connector');
    expect(s.args).toEqual(['serve', 'google_calendar']);
    expect(s.transport).toBe('stdio');
  });

  test('defaults match the Rust defaults (calendar::CalendarSettings, events::Rules)', () => {
    expect(DEFAULT_CALENDAR_SETTINGS.autoRecord).toBe(false);
    expect(DEFAULT_CALENDAR_SETTINGS.autoPrep).toBe(false);
    expect(DEFAULT_CALENDAR_SETTINGS.prepLeadMinutes).toBe(30);
    expect(DEFAULT_CALENDAR_SETTINGS.stopAfterSilenceMinutes).toBe(3);
    expect(DEFAULT_CALENDAR_SETTINGS.rules.maxAttendees).toBe(25);
    expect(DEFAULT_CALENDAR_SETTINGS.rules.includeKeywords).toEqual(['interview']);
  });
});
