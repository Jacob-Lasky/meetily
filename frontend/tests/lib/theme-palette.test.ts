import { describe, expect, test } from 'bun:test';
// Indexed lookups only: enumerating tailwindcss/colors trips its deprecated
// alias getters (warmGray, trueGray ...), which log warnings.
// eslint-disable-next-line @typescript-eslint/no-require-imports
const twColors: Record<string, Record<string, string> | string> = require('tailwindcss/colors');
// eslint-disable-next-line @typescript-eslint/no-require-imports
const palette = require('../../theme-palette.js');

const rgb = (channels: string) => channels.split(' ').map(Number);
const hexChannels = (hex: string) =>
  [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16)).join(' ');

// WCAG relative luminance and contrast ratio.
const luminance = ([r, g, b]: number[]) => {
  const lin = (v: number) => {
    const c = v / 255;
    return c <= 0.03928 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
};
const contrast = (a: string, b: string) => {
  const [hi, lo] = [luminance(rgb(a)), luminance(rgb(b))].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
};
const dark = (v: string) => palette.dark[`--pal-${v}`];

describe('theme palette', () => {
  test('light mode keeps stock Tailwind colours for every remapped class', () => {
    for (const [name, value] of Object.entries(palette.light) as [string, string][]) {
      const key = name.replace(/^--pal-[si]-/, '');
      const [family, shade] = key.split(/-(?=\d+$)/);
      const stock = shade ? (twColors[family] as Record<string, string>)[shade] : (twColors[family] as string);
      expect(value).toBe(hexChannels(stock === '#fff' ? '#ffffff' : stock === '#000' ? '#000000' : stock));
    }
  });

  test('dark mode keeps body text readable on cards and pages', () => {
    for (const surface of ['s-white', 's-gray-50']) {
      expect(contrast(dark('i-gray-900'), dark(surface))).toBeGreaterThan(12);
      expect(contrast(dark('i-gray-700'), dark(surface))).toBeGreaterThan(7);
      expect(contrast(dark('i-gray-600'), dark(surface))).toBeGreaterThan(7);
      expect(contrast(dark('i-gray-500'), dark(surface))).toBeGreaterThan(4.5);
      expect(contrast(dark('i-blue-800'), dark(surface))).toBeGreaterThan(7);
    }
  });

  test('dark mode keeps white labels readable on dark and coloured buttons', () => {
    expect(dark('i-white')).toBe('255 255 255');
    expect(contrast(dark('i-white'), dark('s-gray-900'))).toBeGreaterThan(7);
    expect(contrast(dark('i-white'), dark('s-gray-800'))).toBeGreaterThan(4.5);
    expect(contrast(dark('i-white'), dark('s-blue-600'))).toBeGreaterThan(4.5);
  });

  test('dark tinted surfaces stay dark so their tinted text reads', () => {
    for (const family of ['blue', 'red', 'green', 'amber', 'yellow']) {
      expect(contrast(dark(`i-${family}-800`), dark(`s-${family}-50`))).toBeGreaterThan(7);
    }
  });
});

// Every class string that puts white text on a solid palette background must
// read in both themes. Icon-only controls need the 3:1 non-text ratio, and
// WCAG exempts disabled controls.
describe('white labels on palette backgrounds', () => {
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const fs = require('node:fs');
  // eslint-disable-next-line @typescript-eslint/no-require-imports
  const path = require('node:path');
  const MIN_RATIO: Record<string, number> = {
    'src/components/RecordingControls.tsx:349': 3, // round stop icon, no text
    'src/components/RecordingControls.tsx:374': 1, // disabled icon, exempt
  };
  const files: string[] = [];
  const walk = (dir: string) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else if (p.endsWith('.tsx')) files.push(p);
    }
  };
  const root = path.join(__dirname, '../..');
  walk(path.join(root, 'src'));

  test('meets contrast in light and dark', () => {
    const failures: string[] = [];
    let checked = 0;
    for (const file of files) {
      const rel = path.relative(root, file);
      fs.readFileSync(file, 'utf8').split('\n').forEach((line: string, i: number) => {
        if (!/(^|[\s"'`])text-white(?![\w/-])/.test(line)) return;
        // Unprefixed backgrounds only: hover:/disabled: states are not resting labels.
        for (const m of line.matchAll(/(?:^|[\s"'`])bg-([a-z]+)-(\d{2,3})(?![\w/-])/g)) {
          if (!palette.light[`--pal-s-${m[1]}-${m[2]}`]) continue;
          checked++;
          const where = `${rel}:${i + 1}`;
          const min = MIN_RATIO[where] ?? 4.5;
          for (const mode of ['light', 'dark'] as const) {
            const ratio = contrast(palette[mode]['--pal-i-white'], palette[mode][`--pal-s-${m[1]}-${m[2]}`]);
            if (ratio < min) failures.push(`${where} bg-${m[1]}-${m[2]} ${mode} ${ratio.toFixed(2)}`);
          }
        }
      });
    }
    expect(checked).toBeGreaterThan(20);
    expect(failures).toEqual([]);
  });
});
