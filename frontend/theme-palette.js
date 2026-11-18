// Dark mode by palette remapping.
//
// The UI is written with literal Tailwind palette classes (bg-white,
// text-gray-900, bg-blue-50 ...) in ~60 files. Rather than adding a `dark:`
// twin to every one of them, each palette colour used by the app resolves
// through a CSS variable. In light mode the variable holds Tailwind's stock
// value, so light rendering is byte-for-byte what it was. Under `.dark` the
// variables are remapped.
//
// Two roles are needed because the same class name means different things on
// different utilities:
//   - "surface" (bg, border, ring, divide, gradient, fill, stroke ...):
//     light tints become dark tints, but mid/dark shades stay dark, because
//     `bg-gray-900 text-white` and `bg-blue-600 text-white` buttons must keep
//     their white label readable.
//   - "ink" (text, placeholder): dark shades become light so body copy stays
//     readable, and `text-white` stays white (it sits on coloured buttons).
//
// DO NOT add `dark:` variants for palette colours on top of this: the palette
// is already remapped, so `dark:bg-gray-800` would be applied to the remapped
// value. Use semantic tokens (bg-background, text-foreground ...) or a role
// that already reads correctly in both themes.
const twColors = require('tailwindcss/colors');

const SHADES = ['50', '100', '200', '300', '400', '500', '600', '700', '800', '900', '950'];
const NEUTRALS = ['gray', 'slate', 'neutral', 'zinc', 'stone'];
const CHROMATIC = [
  'red', 'orange', 'amber', 'yellow', 'lime', 'green', 'emerald', 'teal', 'cyan',
  'sky', 'blue', 'indigo', 'violet', 'purple', 'fuchsia', 'pink', 'rose',
];

const hexToRgb = (hex) => {
  const h = hex.replace('#', '');
  return [0, 2, 4].map((i) => parseInt(h.slice(i, i + 2), 16));
};
// mix(a, b, t): t=0 gives a, t=1 gives b.
const mix = (a, b, t) => {
  const [ra, ga, ba] = hexToRgb(a);
  const [rb, gb, bb] = hexToRgb(b);
  return [ra + (rb - ra) * t, ga + (gb - ga) * t, ba + (bb - ba) * t].map(Math.round);
};
const channels = (rgb) => (typeof rgb === 'string' ? hexToRgb(rgb) : rgb).join(' ');

// Dark-mode values for a neutral family. Ordering is kept so that a light
// page (gray-50) under white cards becomes a dark page under lifted cards.
function neutralDark(f) {
  const c = twColors[f];
  return {
    surface: {
      white: mix(c[900], c[800], 0.35),
      50: mix(c[900], c[950], 0.45),
      100: mix(c[900], c[800], 0.7),
      200: c[800],
      300: mix(c[800], c[700], 0.6),
      400: c[700],
      500: c[500],
      600: c[600],
      700: mix(c[700], c[600], 0.5),
      // bg-gray-900 / hover:bg-gray-800 are dark primary buttons with white
      // labels: keep them dark, lifted just enough to stand off the page.
      800: c[600],
      900: c[700],
      950: c[800],
    },
    ink: {
      black: c[50],
      950: c[50],
      900: c[100],
      800: c[200],
      700: c[300],
      600: mix(c[300], c[400], 0.45),
      500: c[400],
      400: mix(c[400], c[500], 0.5),
      300: c[600],
      200: c[700],
      100: c[800],
      50: c[900],
    },
  };
}

// Dark-mode values for a chromatic family: tints become a wash of the colour
// over the dark card surface; text shades lighten. Mid shades are unchanged.
function chromaticDark(f, card) {
  const c = twColors[f];
  const wash = (t) => mix(card, c[500], t);
  return {
    surface: { 50: wash(0.12), 100: wash(0.18), 200: wash(0.28), 300: wash(0.4) },
    ink: { 600: c[400], 700: c[300], 800: c[200], 900: c[200], 950: c[100] },
  };
}

function build() {
  const surfaceColors = {};
  const inkColors = {};
  const light = {};
  const dark = {};
  const ref = (role, name) => `rgb(var(--pal-${role}-${name}) / <alpha-value>)`;

  const register = (family, shade, value, darkSurface, darkInk) => {
    const name = shade ? `${family}-${shade}` : family;
    light[`--pal-s-${name}`] = channels(value);
    light[`--pal-i-${name}`] = channels(value);
    dark[`--pal-s-${name}`] = channels(darkSurface ?? value);
    dark[`--pal-i-${name}`] = channels(darkInk ?? value);
    if (shade) {
      (surfaceColors[family] ||= {})[shade] = ref('s', name);
      (inkColors[family] ||= {})[shade] = ref('i', name);
    } else {
      surfaceColors[family] = ref('s', name);
      inkColors[family] = ref('i', name);
    }
  };

  const grayDark = neutralDark('gray');
  // The app's cards and pages are gray-toned; white and black follow gray.
  register('white', null, '#ffffff', grayDark.surface.white, null);
  register('black', null, '#000000', null, grayDark.ink.black);

  for (const f of NEUTRALS) {
    const d = neutralDark(f);
    for (const s of SHADES) register(f, s, twColors[f][s], d.surface[s], d.ink[s]);
  }
  const card = '#' + grayDark.surface.white.map((v) => v.toString(16).padStart(2, '0')).join('');
  for (const f of CHROMATIC) {
    const d = chromaticDark(f, card);
    for (const s of SHADES) register(f, s, twColors[f][s], d.surface[s], d.ink[s]);
  }

  return { surfaceColors, inkColors, light, dark };
}

module.exports = build();
