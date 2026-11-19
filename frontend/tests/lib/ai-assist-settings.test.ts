import { describe, expect, test } from 'bun:test';
import {
  briefEntry,
  deepHiveServer,
  formatPairs,
  joinCommand,
  parsePairs,
  splitCommand,
} from '../../src/lib/ai-assist-settings';

describe('AI Assist settings helpers', () => {
  test('command lines split on whitespace and group quotes, without a shell', () => {
    expect(splitCommand('/usr/bin/mcp serve deephive')).toEqual(['/usr/bin/mcp', 'serve', 'deephive']);
    expect(splitCommand(`npx -y "@scope/server" --dir '/a b/c'`)).toEqual(['npx', '-y', '@scope/server', '--dir', '/a b/c']);
    expect(splitCommand('  ')).toEqual([]);
    expect(splitCommand(`a ""`)).toEqual(['a', '']);
    expect(splitCommand('echo $HOME; rm -rf x')).toEqual(['echo', '$HOME;', 'rm', '-rf', 'x']);
  });

  test('joinCommand round-trips through splitCommand', () => {
    for (const parts of [['/bin/x', 'serve', 'a b'], ['npx', '-y', "it's"], ['/bin/x', '']]) {
      const [command, ...args] = parts;
      expect(splitCommand(joinCommand(command, args))).toEqual(parts);
    }
  });

  test('env and header pairs parse and format', () => {
    expect(parsePairs('A=1\nB = two=2\nnoseparator\n=nokey', '=')).toEqual({ A: '1', B: 'two=2' });
    expect(parsePairs('Authorization: Bearer x:y\nX-Org:deepgram', ':')).toEqual({
      Authorization: 'Bearer x:y',
      'X-Org': 'deepgram',
    });
    expect(formatPairs({ A: '1' }, '=')).toBe('A=1');
    expect(formatPairs({ Authorization: 'Bearer x' }, ':')).toBe('Authorization: Bearer x');
  });

  test('the DeepHive preset is the stdio relay limited to search_knowledge', () => {
    const s = deepHiveServer('/home/u/');
    expect(s.command).toBe('/home/u/.claude/skills/mcp-connector/scripts/mcp-connector');
    expect(s.args).toEqual(['serve', 'deephive']);
    expect(s.transport).toBe('stdio');
    expect(s.allowedTools).toEqual(['search_knowledge']);
  });

  test('a brief entry names the meeting and its date', () => {
    const entry = briefEntry({ id: 'm', title: 'Qualtrics prep', created_at: '2026-11-09T19:54:58Z', summary: 'Plan.' });
    expect(entry).toBe('Earlier meeting "Qualtrics prep" (2026-11-09):\nPlan.');
    expect(briefEntry({ id: 'm', title: 't', created_at: '2026-11-09', summary: '' })).toContain('(no summary yet)');
  });
});
