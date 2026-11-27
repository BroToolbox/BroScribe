"""BroScribe colour system: generate the scales and check every pair that matters.

usage:
  python3 scripts/palette.py            check crates/broscribe-app/assets/brand/color.css
  python3 scripts/palette.py --write    regenerate the scales block in color.css, then check
  python3 scripts/palette.py --all      also print every checked pair
  python3 scripts/palette.py --theme-json
                                     also write the app's theme (crates/broscribe-app/assets/themes/broscribe.json)
                                     and its brand table (crates/broscribe-app/src/ui/palette_gen.rs)
  python3 scripts/palette.py --check-generated
                                     fail when the app's theme or brand table differs from what --theme-json writes

The scales are twelve steps per hue, built in OKLCH so that a step means the same thing in every
hue and both themes: 1-2 grounds, 3-5 element fills (rest, hover, selected), 6-8 borders,
9-10 solid fills, 11 low-contrast text, 12 high-contrast text. The hues come from the BroScribe kit:
violet from the headphones (297), teal from the parent brand, turned towards cyan (224) so it stays apart
from crimson for people with protanopia, crimson from the kit's record dot (12), plus amber (78)
for decisions and blue (260) for information. The neutral scale is the
kit's slate with almost all of its colour taken out (296).
"""
import itertools
import math
import os
import re
import sys

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
CSS = os.path.join(ROOT, 'crates', 'broscribe-app', 'assets', 'brand', 'color.css')

# ── OKLCH → sRGB ──


def _enc(c):
    c = max(0.0, min(1.0, c))
    return 12.92 * c if c <= 0.0031308 else 1.055 * c ** (1 / 2.4) - 0.055


def _lin_rgb(L, C, H):
    a, b = C * math.cos(math.radians(H)), C * math.sin(math.radians(H))
    l_ = L + 0.3963377774 * a + 0.2158037573 * b
    m_ = L - 0.1055613458 * a - 0.0638541728 * b
    s_ = L - 0.0894841775 * a - 1.2914855480 * b
    l, m, s = l_ ** 3, m_ ** 3, s_ ** 3
    return (4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
            -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
            -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s)


def oklch(L, C, H):
    """Hex for an OKLCH colour, reducing chroma until it fits in sRGB."""
    while C > 0:
        r, g, b = _lin_rgb(L, C, H)
        if all(-1e-4 <= x <= 1 + 1e-4 for x in (r, g, b)):
            break
        C -= 0.002
    r, g, b = _lin_rgb(L, max(C, 0), H)
    return '#' + ''.join(f'{round(_enc(x) * 255):02X}' for x in (r, g, b))


# ── Scales ──
# Lightness of steps 1-8 (grounds, fills, borders) and the share of the hue's ground chroma each gets.
BG_L = {'light': [0.990, 0.978, 0.962, 0.945, 0.928, 0.905, 0.870, 0.800],
        'dark': [0.195, 0.232, 0.252, 0.283, 0.312, 0.342, 0.392, 0.472]}
BG_F = [0.10, 0.18, 0.34, 0.47, 0.58, 0.66, 0.74, 0.86]

# name: hue (or {theme: hue}), ground chroma (or {theme: chroma}), {theme: (L9, C9, L10, L11, C11, L12, C12)}
# Neutral is warm stone in light, from the mascot's cream shell (80), and a soft charcoal in dark with a
# trace of the visor's violet (285), never black.
SCALES = {
    'neutral': ({'light': 80, 'dark': 290}, {'light': 0.020, 'dark': ('flat', 0.012)}, {'light': (0.600, 0.014, 0.555, 0.470, 0.016, 0.240, 0.012),
                         'dark': (0.590, 0.014, 0.640, 0.790, 0.012, 0.955, 0.006)}),
    'violet': (297, 0.115, {'light': (0.535, 0.215, 0.490, 0.470, 0.180, 0.300, 0.110),
                            'dark': (0.700, 0.165, 0.750, 0.800, 0.120, 0.945, 0.040)}),
    'crimson': (12, 0.095, {'light': (0.560, 0.200, 0.515, 0.480, 0.170, 0.300, 0.090),
                            'dark': (0.690, 0.175, 0.740, 0.740, 0.140, 0.945, 0.035)}),
    'amber': (78, 0.120, {'light': (0.820, 0.165, 0.775, 0.480, 0.105, 0.310, 0.060),
                          'dark': (0.820, 0.165, 0.860, 0.875, 0.135, 0.955, 0.040)}),
    'teal': (224, 0.075, {'light': (0.600, 0.115, 0.555, 0.465, 0.100, 0.290, 0.055),
                          'dark': (0.730, 0.120, 0.780, 0.820, 0.105, 0.950, 0.035)}),
    'blue': (260, 0.085, {'light': (0.555, 0.175, 0.510, 0.470, 0.150, 0.290, 0.080),
                          'dark': (0.690, 0.150, 0.740, 0.800, 0.110, 0.950, 0.030)}),
}
# Person hues: (name, hue). Tint behind the initials and the ink for the initials and the name.
# Kept away from the state hues (crimson 12, amber 78, teal 224, blue 260) and the violet (297), so a
# disc is never read as a state; nothing sits between 180 and 320. Only the disc carries the hue.
# The umber is a low-chroma brown so it never reads as the amber of decisions.
PEOPLE = [('copper', 38), ('umber', 66, 0.085), ('olive', 108), ('green', 142), ('jade', 168), ('orchid', 328),
          ('rose', 356)]
# Person avatars are solid discs: light (L, C) with white initials, dark (L, C) with charcoal initials.
# The same colour writes the person's name, so it must read on cards and on the desk.
PEOPLE_LC = {'light': (0.520, 0.130), 'dark': (0.770, 0.110)}


def scales():
    out = {'light': {}, 'dark': {}}
    for theme in ('light', 'dark'):
        for name, (hue0, cbg0, solid) in SCALES.items():
            hue, cbg = hue0, cbg0
            L9, C9, L10, L11, C11, L12, C12 = solid[theme]
            hue = hue[theme] if isinstance(hue, dict) else hue
            cbg = cbg[theme] if isinstance(cbg, dict) else cbg
            for i in range(8):
                c = cbg[1] if isinstance(cbg, tuple) else cbg * BG_F[i]
                out[theme][f'{name}-{i + 1}'] = oklch(BG_L[theme][i], c, hue)
            out[theme][f'{name}-9'] = oklch(L9, C9, hue)
            out[theme][f'{name}-10'] = oklch(L10, C9, hue)
            out[theme][f'{name}-11'] = oklch(L11, C11, hue)
            out[theme][f'{name}-12'] = oklch(L12, C12, hue)
        pL, pC = PEOPLE_LC[theme]
        for n, (label, hue, *c) in enumerate(PEOPLE, 1):
            out[theme][f'hue-{n}'] = oklch(pL, c[0] if c else pC, hue)
    return out


def scales_css():
    sc = scales()

    def block(theme):
        lines = []
        for name in SCALES:
            lines.append('  ' + ' '.join(f'--{name}-{i}: {sc[theme][f"{name}-{i}"]};' for i in range(1, 13)))
        for n, (label, *_) in enumerate(PEOPLE, 1):
            lines.append(f'  --hue-{n}: {sc[theme][f"hue-{n}"]};   /* {label} */')
        return '\n'.join(lines)
    return ('/* scales:begin — generated by scripts/palette.py --write; do not edit by hand */\n'
            'html:not([data-palette="graphite"]) {\n' + block('light') + '\n}\n'
            'html:not([data-palette="graphite"])[data-theme="dark"] {\n' + block('dark') + '\n}\n'
            '/* scales:end */')


def write():
    css = open(CSS).read()
    new = re.sub(r'/\* scales:begin.*?/\* scales:end \*/', lambda m: scales_css(), css, flags=re.S)
    if new == css and 'scales:begin' not in css:
        sys.exit('color.css has no scales:begin / scales:end markers')
    open(CSS, 'w').write(new)


# ── Reading the semantic tokens back ──


def _blocks(css, more=False):
    """Light and dark tokens. more=True applies the prefers-contrast: more overrides on top."""
    css = re.sub(r'/\*.*?\*/', '', css, flags=re.S)
    media = ''.join(re.findall(r'@media\s*\(prefers-contrast:\s*more\)\s*\{((?:[^{}]*\{[^{}]*\})*\s*)\}', css))
    css = re.sub(r'@media[^{]*\{(?:[^{}]*\{[^{}]*\})*\s*\}', '', css)
    light, dark = {}, {}

    def take(text):
        for sel, body in re.findall(r'([^{}]+)\{([^{}]*)\}', text):
            sel = sel.strip()
            decls = dict(re.findall(r'--([a-z0-9-]+)\s*:\s*([^;]+);', body))
            if sel == 'html:not([data-palette="graphite"])':
                light.update(decls)
            elif sel == 'html:not([data-palette="graphite"])[data-theme="dark"]':
                dark.update(decls)
    take(css)
    if more:
        take(media)
    return light, {**light, **dark}


def _resolve(tokens, value, depth=0):
    value = value.strip()
    m = re.fullmatch(r'var\(--([a-z0-9-]+)\)', value)
    if m and depth < 10:
        return _resolve(tokens, tokens.get(m.group(1), ''), depth + 1)
    m = re.fullmatch(r'#([0-9A-Fa-f]{6})', value)
    return '#' + m.group(1).upper() if m else None


def load(more=False):
    light, dark = _blocks(open(CSS).read(), more)
    res = {}
    for theme, toks in (('light', light), ('dark', dark)):
        res[theme] = {k.replace('-', '_'): v for k in toks if (v := _resolve(toks, toks[k]))}
    return res


# ── Contrast and colour-vision checks ──


def _lin(c):
    c = c / 255
    return c / 12.92 if c <= 0.04045 else ((c + 0.055) / 1.055) ** 2.4


def _rgb(h):
    h = h.lstrip('#')
    return tuple(int(h[i:i + 2], 16) for i in (0, 2, 4))


def lum(h):
    r, g, b = (_lin(x) for x in _rgb(h))
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def cr(a, b):
    la, lb = lum(a), lum(b)
    return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)


MACHADO = {
    'protan': [[0.152286, 1.052583, -0.204868], [0.114503, 0.786281, 0.099216], [-0.003882, -0.048116, 1.051998]],
    'deutan': [[0.367322, 0.860646, -0.227968], [0.280085, 0.672501, 0.047413], [-0.011820, 0.042940, 0.968881]],
    'tritan': [[1.255528, -0.076749, -0.178779], [-0.078411, 0.930809, 0.147602], [0.004733, 0.691367, 0.303900]],
}


def _lab(v):
    r, g, b = v
    x, y, z = (0.4124 * r + 0.3576 * g + 0.1805 * b, 0.2126 * r + 0.7152 * g + 0.0722 * b,
               0.0193 * r + 0.1192 * g + 0.9505 * b)
    f = lambda t: t ** (1 / 3) if t > 0.008856 else 7.787 * t + 16 / 116
    fx, fy, fz = f(x / 0.95047), f(y), f(z / 1.08883)
    return (116 * fy - 16, 500 * (fx - fy), 200 * (fy - fz))


def seen(h, kind):
    v = [_lin(x) for x in _rgb(h)]
    if kind == 'none':
        return _lab(v)
    m = MACHADO[kind]
    return _lab([max(0, min(1, sum(m[i][j] * v[j] for j in range(3)))) for i in range(3)])


def de(a, b):
    return sum((p - q) ** 2 for p, q in zip(a, b)) ** 0.5


GROUNDS = ['background', 'chrome', 'tray', 'sidebar', 'surface', 'popover', 'muted']
RULES = [
    # (foreground token, grounds, minimum, what)
    ('foreground', GROUNDS + ['selected', 'sidebar_selected'], 7, 'body text'),
    ('muted_foreground', GROUNDS, 4.5, 'secondary text'),
    ('selected_foreground', ['selected', 'sidebar_selected'], 7, 'text on a selection'),
    ('link', ['background', 'chrome', 'tray', 'surface', 'popover', 'muted', 'ai_soft'], 4.5, 'links'),
    ('primary_foreground', ['primary', 'primary_hover', 'primary_active'], 4.5, 'label on primary'),
    ('primary', ['background', 'chrome', 'tray', 'sidebar', 'sidebar_selected', 'selected'], 3, 'accent mark'),
    ('ring', ['background', 'chrome', 'tray', 'sidebar', 'surface', 'popover'], 3, 'focus ring'),
    ('input_border', ['background', 'chrome', 'tray', 'surface', 'popover'], 3, 'control edge'),
    ('danger', ['background', 'chrome', 'tray', 'surface', 'popover', 'muted', 'danger_soft'], 4.5, 'failure text'),
    ('warning', ['background', 'chrome', 'tray', 'surface', 'popover', 'muted', 'warning_soft'], 4.5, 'decision text'),
    ('success', ['background', 'chrome', 'tray', 'surface', 'popover', 'muted', 'success_soft'], 4.5, 'done text'),
    ('info', ['background', 'tray', 'surface', 'info_soft'], 4.5, 'information text'),
    ('danger_foreground', ['danger'], 4.5, 'label on a destructive button'),
    ('foreground', ['danger_soft', 'warning_soft', 'success_soft', 'info_soft', 'ai_soft'], 7, 'text on a tinted fill'),
    ('ai_foreground', ['ai_soft', 'tray', 'background'], 4.5, 'AI label'),
    ('recording', ['background', 'chrome', 'tray', 'sidebar'], 4.5, 'Recording word'),
    ('meter_me', ['meter_track'], 3, 'meter'),
    ('meter_others', ['meter_track'], 3, 'meter'),
    ('section_today', ['sidebar', 'sidebar_selected'], 3, 'section icon'),
    ('section_meetings', ['sidebar', 'sidebar_selected'], 3, 'section icon'),
    ('section_people', ['sidebar', 'sidebar_selected'], 3, 'section icon'),
    ('section_review', ['sidebar', 'sidebar_selected'], 3, 'section icon'),
    ('badge_foreground', ['badge'], 4.5, 'Review count'),
    ('visor_foreground', ['visor'], 7, 'visor words'),
    ('visor_accent', ['visor'], 4.5, 'visor marks'),
    ('visor_recording', ['visor'], 4.5, 'visor recording'),
    ('find_foreground', ['find'], 7, 'text in a find highlight'),
    ('visor_accent', ['visor'], 3, 'focus ring on the visor'),
    ('primary', ['selected', 'card', 'sidebar_selected'], 3, 'selected-row bar'),
    ('on_person', ['me'], 4.5, 'initials on the Me disc'),
]


def check(verbose):
    fails, rows = [], 0
    for mode, theme in itertools.product(('standard', 'more contrast'), ('light', 'dark')):
        t = load(mode != 'standard')[theme]
        theme_name = theme
        theme = theme if mode == 'standard' else theme + ' (more contrast)'
        for fg, grounds, minimum, what in RULES:
            for bg in grounds:
                if fg not in t or bg not in t:
                    fails.append(f'{theme}: missing token {fg if fg not in t else bg}')
                    continue
                r = cr(t[fg], t[bg])
                rows += 1
                bad = r < minimum
                if verbose or bad:
                    print(f'{theme:5} {what:30} {fg:20} on {bg:18} {r:6.2f} (min {minimum}){"  <-- FAIL" if bad else ""}')
                if bad:
                    fails.append(f'{theme}: {fg} on {bg} {r:.2f} < {minimum} ({what})')
        for n in range(1, len(PEOPLE) + 1):
            hc, ink = t[f'hue_{n}'], t['on_person']
            rows += 3
            if cr(ink, hc) < 4.5:
                fails.append(f'{theme}: person hue {n} initials {cr(ink, hc):.2f}')
            for g in ('background', 'tray'):
                if cr(hc, t[g]) < 4.5:
                    fails.append(f'{theme}: person hue {n} name on {g} {cr(hc, t[g]):.2f}')
        worst = None
        for kind in ('protan', 'deutan', 'tritan'):
            for a, b in itertools.combinations(('danger', 'warning', 'success'), 2):
                d = de(seen(t[a], kind), seen(t[b], kind))
                if worst is None or d < worst[0]:
                    worst = (d, a, b, kind)
        soft = min((de(seen(t[a], k), seen(t[b], k)), a, b, k) for k in ('protan', 'deutan', 'tritan')
                   for a, b in itertools.combinations(('danger_soft', 'warning_soft', 'success_soft'), 2))
        if soft[0] < 5:
            fails.append(f'{theme}: tinted fills {soft[1]} and {soft[2]} merge under {soft[3]} ({soft[0]:.1f})')
        print(f'{theme}: smallest distance between two status colours under a colour-vision deficiency: '
              f'{worst[0]:.1f} ({worst[1]} and {worst[2]}, {worst[3]})')
        if worst[0] < (20 if mode == 'standard' else 15):   # Increase Contrast trades a little hue for contrast
            fails.append(f'{theme}: status colours too close under {worst[3]} ({worst[0]:.1f})')
    print(f'{rows} contrast pairs checked.')
    if fails:
        print('FAILS:', *fails, sep='\n  ')
        sys.exit(1)
    print('All checks pass.')


# ── The app's theme: gpui-component keys and the brand table ──
#
# The app never reads color.css at run time. This writes the two files it builds from: gpui-component's theme-set JSON
# (four variants: light, dark and their Increase Contrast versions) and a Rust table of the roles
# gpui-component has no key for. Each key below was mapped after reading its use in gpui-component 0.10.

THEME_JSON = os.path.join(ROOT, 'crates', 'broscribe-app', 'assets', 'themes', 'broscribe.json')
PALETTE_RS = os.path.join(ROOT, 'crates', 'broscribe-app', 'src', 'ui', 'palette_gen.rs')
VARIANTS = [('BroScribe Light', 'light', False), ('BroScribe Dark', 'dark', False),
            ('BroScribe Light (more contrast)', 'light', True), ('BroScribe Dark (more contrast)', 'dark', True)]


def _to_oklch(h):
    r, g, b = (_lin(x) for x in _rgb(h))
    l = (0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b) ** (1 / 3)
    m = (0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b) ** (1 / 3)
    s = (0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b) ** (1 / 3)
    L = 0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s
    a = 1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s
    bb = 0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s
    return L, math.hypot(a, bb), math.degrees(math.atan2(bb, a)) % 360


def toward_label(fill, label, step):
    """`fill` moved `step` in OKLCH lightness away from `label`: a hover or pressed fill whose label
    contrast rises with each step."""
    L, C, H = _to_oklch(fill)
    d = -step if lum(label) > lum(fill) else step
    return oklch(min(1, max(0, L + d)), C, H)


def blend_hex(top, alpha, ground):
    """`top` at `alpha` over `ground`, as GPUI composites it (on the encoded channels)."""
    t, g = _rgb(top), _rgb(ground)
    return '#' + ''.join(f'{round(t[i] * alpha + g[i] * (1 - alpha)):02X}' for i in range(3))


def _alpha_hex(value):
    """A role written as #RRGGBBAA, split into its colour and alpha."""
    m = re.fullmatch(r'#([0-9A-Fa-f]{6})([0-9A-Fa-f]{2})', value.strip())
    return ('#' + m.group(1).upper(), int(m.group(2), 16) / 255) if m else None


def raw_tokens(more):
    light, dark = _blocks(open(CSS).read(), more)
    return {'light': light, 'dark': dark}


def theme_colors(theme, more):
    """gpui-component's keys for one variant."""
    t = load(more)[theme]
    raw = raw_tokens(more)[theme]
    sc = scales()[theme]
    neutral = lambda n: sc[f'neutral-{n}']
    # The pressed step of a neutral control: one border step past its hover fill.
    pressed = neutral(7) if theme == 'light' else neutral(8)
    c = {}

    def solid(key, fill, label):
        c[f'{key}.background'] = fill
        c[f'{key}.hover.background'] = toward_label(fill, label, 0.04)
        c[f'{key}.active.background'] = toward_label(fill, label, 0.08)
        c[f'{key}.foreground'] = label

    def alpha(role, opaque):
        """A translucent role; under Increase Contrast it is drawn opaque over the desk, which is also
        what Reduce Transparency takes from the contrast variant."""
        colour, a = _alpha_hex(raw[role.replace('_', '-')])
        if opaque:
            return blend_hex(colour, a, t['tray'])
        return colour + f'{round(a * 255):02X}'

    label_on_state = t['danger_foreground']
    # Grounds: the desk is every window's ground (title bar and status bar included); the sidebar the deepest.
    c['background'] = t['tray']
    c['title_bar.background'] = t['tray']
    c['title_bar.border'] = t['border']
    c['status_bar.background'] = t['tray']
    c['status_bar.border'] = t['border']
    c['window.border'] = t['border']
    c['sidebar.background'] = t['sidebar']
    c['sidebar.border'] = t['border']
    c['sidebar.foreground'] = t['foreground']
    c['sidebar.accent.background'] = t['sidebar_selected']
    c['sidebar.accent.foreground'] = t['foreground']
    c['sidebar.primary.background'] = t['primary']
    c['sidebar.primary.foreground'] = t['primary_foreground']
    c['popover.background'] = t['popover']
    c['popover.foreground'] = t['foreground']
    # Cards: group boxes, accordions, lists and tables are blocks of content.
    c['group_box.background'] = t['card']
    c['group_box.foreground'] = t['foreground']
    c['group_box.title.foreground'] = t['foreground']
    c['accordion.background'] = t['card']
    c['list.background'] = t['card']
    c['list.head.background'] = t['surface_inset']
    c['list.hover.background'] = t['muted']
    c['list.active.background'] = t['selected']
    c['list.active.border'] = t['primary']
    c['list.highlight.foreground'] = t['foreground']
    c['table.background'] = t['card']
    c['table.head.background'] = t['surface_inset']
    c['table.head.foreground'] = t['foreground']
    c['table.foot.background'] = t['surface_inset']
    c['table.foot.foreground'] = t['foreground']
    c['table.hover.background'] = t['muted']
    c['table.active.background'] = t['selected']
    c['table.active.border'] = t['primary']
    c['table.row.border'] = t['border']
    c['description_list.label.background'] = t['surface_inset']
    c['description_list.label.foreground'] = t['foreground']
    # Tabs: the bar is an inset box; the selected tab is card-coloured.
    c['tab_bar.background'] = t['surface_inset']
    c['tab_bar.segmented.background'] = t['surface_inset']
    c['tab.background'] = t['surface_inset']
    c['tab.foreground'] = t['muted_foreground']
    c['tab.active.background'] = t['card']
    c['tab.active.foreground'] = t['foreground']
    # Text and edges.
    c['foreground'] = t['foreground']
    c['muted.background'] = t['muted']
    c['muted.foreground'] = t['muted_foreground']
    c['border'] = t['border']
    c['input.border'] = t['input_border']
    c['caret'] = t['foreground']
    c['ring'] = t['ring']
    c['link'] = t['link']
    c['link.hover'] = t['selected_foreground']
    c['link.active'] = t['selected_foreground']
    c['selection.background'] = alpha('selection', False)
    c['overlay'] = alpha('overlay', more)
    c['drop_target.background'] = alpha('drop_target', more)
    c['drag.border'] = t['primary']
    # Quiet fills: secondary buttons, menu and list hover (accent), skeletons.
    for key in ('button', 'button.secondary', 'secondary'):
        c[f'{key}.background'] = t['muted']
        c[f'{key}.hover.background'] = t['muted_hover']
        c[f'{key}.active.background'] = pressed
        c[f'{key}.foreground'] = t['foreground']
    c['accent.background'] = t['muted']
    c['accent.foreground'] = t['foreground']
    c['skeleton.background'] = t['muted']
    # BroScribe: the one primary action, checked controls, progress.
    for key in ('primary', 'button.primary'):
        c[f'{key}.background'] = t['primary']
        c[f'{key}.hover.background'] = t['primary_hover']
        c[f'{key}.active.background'] = t['primary_active']
        c[f'{key}.foreground'] = t['primary_foreground']
    c['progress.bar.background'] = t['primary']
    c['slider.background'] = t['primary']
    c['slider.thumb.background'] = t['card']
    # A switch's checked track is gpui-component's `primary`; `switch.background` is the unchecked
    # track, a control edge.
    c['switch.background'] = t['input_border']
    c['switch.thumb.background'] = '#FFFFFF' if theme == 'light' else neutral(1)
    # States: solid fills whose colour is also their text colour (gpui-component writes `danger`
    # as text in alerts, notifications and invalid fields).
    for key, role in (('danger', 'danger'), ('warning', 'warning'), ('success', 'success'), ('info', 'info')):
        solid(key, t[role], label_on_state)
        solid(f'button.{key}', t[role], label_on_state)
    c['base.red'] = t['danger']
    c['base.yellow'] = t['warning']
    c['base.green'] = t['success']
    c['scrollbar.background'] = '#00000000'
    c['scrollbar.thumb.background'] = t['input_border']
    c['scrollbar.thumb.hover.background'] = t['muted_foreground']
    return {k: c[k].lower() for k in sorted(c)}


def theme_json():
    themes = []
    for name, theme, more in VARIANTS:
        themes.append({'is_default': name == 'BroScribe Light', 'name': name, 'mode': theme,
                       'font.family': 'Inter', 'font.size': 16, 'mono_font.size': 13,
                       'radius': 8, 'radius.lg': 12, 'shadow': True, 'colors': theme_colors(theme, more)})
    import json
    return json.dumps({'name': 'BroScribe', 'author': 'BroScribe', 'themes': themes}, indent=2) + '\n'


# Roles with no gpui-component key, in the order of the Rust struct. Each is a light and a dark value.
BRAND_ROLES = [
    ('card', 'Every block of content on the desk'),
    ('surface_inset', 'A box inside a card: the desk colour'),
    ('sidebar_selected', 'The selected item of a sidebar: a card-coloured pill'),
    ('chrome', 'Toolbars and window chrome'),
    ('foreground', 'Body text'),
    ('primary', 'BroScribe violet: the primary action, selection, focus, progress'),
    ('selected', 'The selected row’s tint'),
    ('selected_foreground', 'Text on a selection'),
    ('danger', 'Failure text and marks'),
    ('danger_soft', 'A failure banner’s fill'),
    ('danger_border', 'A failure banner’s edge'),
    ('warning', 'Decision text and marks'),
    ('warning_soft', 'An attention banner’s fill, unresolved badges'),
    ('warning_border', 'An attention banner’s edge'),
    ('success', 'Done text and marks'),
    ('success_soft', 'A done banner’s fill, the Named by you badge'),
    ('success_border', 'A done banner’s edge'),
    ('info', 'Information text and marks'),
    ('info_soft', 'An information banner’s fill'),
    ('info_border', 'An information banner’s edge'),
    ('ai_soft', 'Behind what an AI provider wrote'),
    ('ai_border', 'The violet rule beside what an AI provider wrote'),
    ('ai_foreground', 'The AI caution line'),
    ('badge', 'The Review count'),
    ('badge_foreground', 'The Review count’s number'),
    ('me', 'The Me disc: slate'),
    ('on_person', 'Initials on a person’s disc'),
    ('recording', 'The capture dot and the word Recording'),
    ('meter_me', 'Lit Me (microphone) meter segments'),
    ('meter_others', 'Lit Others (system audio) meter segments'),
    ('meter_track', 'Unlit meter segments'),
    ('find', 'A find highlight'),
    ('find_foreground', 'Text in a find highlight'),
    ('visor', 'The visor: the one dark panel'),
    ('visor_rim', 'The visor’s edge'),
    ('visor_foreground', 'Eyes and words in the visor'),
    ('visor_accent', 'Marks and the focus ring in the visor'),
    ('visor_recording', 'The record dot in the visor'),
]
SHADOWS = [('shadow_card', 'A card on the desk'), ('shadow_pill', 'The selected sidebar pill'),
           ('shadow_primary', 'The primary button'), ('shadow_popover', 'Menus, popovers and dialogs')]


def _split_top(value):
    parts, depth, cur = [], 0, ''
    for ch in value:
        depth += ch == '('
        depth -= ch == ')'
        if ch == ',' and depth == 0:
            parts.append(cur)
            cur = ''
        else:
            cur += ch
    return [p.strip() for p in parts + [cur] if p.strip()]


def shadow_layers(raw, value, resolved):
    value = value.strip()
    if value == 'none':
        return []
    layers = []
    for part in _split_top(value):
        m = re.search(r'rgba\((\d+),\s*(\d+),\s*(\d+),\s*([\d.]+)\)', part)
        if m:
            rgba = (int(m.group(1)) << 24) | (int(m.group(2)) << 16) | (int(m.group(3)) << 8) | round(float(m.group(4)) * 255)
            part = part[:m.start()]
        else:
            m = re.search(r'var\(--([a-z0-9-]+)\)', part)
            hexv = resolved[m.group(1).replace('-', '_')]
            rgba = (int(hexv[1:], 16) << 8) | 0xFF
            part = part[:m.start()]
        nums = [float(n.replace('px', '')) for n in part.split()]
        nums += [0.0] * (4 - len(nums))
        layers.append((*nums[:4], rgba))
    return layers


def _f(x):
    return f'{x:.1f}'


def palette_rs():
    out = ['//! Generated by `scripts/palette.py --theme-json` from',
           '//! `crates/broscribe-app/assets/brand/color.css`. Do not edit by hand: change the CSS or the',
           '//! script and run it again (`--check-generated` fails while the two disagree).', '',
           '/// One layer of a box shadow: offset, blur and spread in pixels, colour as `0xRRGGBBAA`.',
           '#[derive(Clone, Copy, Debug, PartialEq)]',
           'pub struct ShadowLayer {', '    pub x: f32,', '    pub y: f32,', '    pub blur: f32,',
           '    pub spread: f32,', '    pub rgba: u32,', '}', '',
           '/// The roles gpui-component has no key for, as `0xRRGGBB`, for one variant.',
           '#[derive(Clone, Copy, Debug, PartialEq)]', 'pub struct Palette {']
    for role, what in BRAND_ROLES:
        out += [f'    /// {what}.', f'    pub {role}: u32,']
    out += ['    /// Person hues 1 to 7 (copper, umber, olive, green, jade, orchid, rose): avatar discs only.',
            '    pub hues: [u32; 7],']
    for role, what in SHADOWS:
        out += [f'    /// {what}.', f'    pub {role}: &\'static [ShadowLayer],']
    out += ['}', '', '/// The card radius, in pixels.', f'pub const RADIUS_CARD: f32 = {_f(float(re.fullmatch(r"([\d.]+)rem", raw_tokens(False)["light"]["radius-card"].strip()).group(1)) * 16)};', '']
    names = {('light', False): 'LIGHT', ('dark', False): 'DARK', ('light', True): 'LIGHT_CONTRAST', ('dark', True): 'DARK_CONTRAST'}
    roles_tables = []
    for name, theme, more in VARIANTS:
        t = load(more)[theme]
        raw = raw_tokens(more)[theme]
        const = names[(theme, more)]
        out.append(f'/// {name}.')
        out.append(f'pub const {const}: Palette = Palette {{')
        for role, _ in BRAND_ROLES:
            out.append(f'    {role}: 0x{t[role][1:].lower()},')
        out.append('    hues: [' + ', '.join(f'0x{t[f"hue_{n}"][1:].lower()}' for n in range(1, 8)) + '],')
        for role, _ in SHADOWS:
            layers = shadow_layers(raw, raw[role.replace('_', '-')], t)
            inner = ', '.join(f'ShadowLayer {{ x: {_f(x)}, y: {_f(y)}, blur: {_f(b)}, spread: {_f(sp)}, rgba: 0x{rgba:08x} }}'
                              for x, y, b, sp, rgba in layers)
            out.append(f'    {role}: &[{inner}],')
        out.append('};')
        out.append('')
        roles_tables.append((name, t))
    out += ['/// Every resolved role of each variant, by its CSS name with `_` for `-`: what the contrast and',
            '/// colour-vision tests check.', '#[cfg(test)]', 'pub const ROLES: [(&str, &[(&str, u32)]); 4] = [']
    for name, t in roles_tables:
        out.append(f'    (\n        "{name}",\n        &[')
        for k in sorted(t):
            out.append(f'            ("{k}", 0x{t[k][1:].lower()}),')
        out.append('        ],\n    ),')
    out.append('];')
    return '\n'.join(out) + '\n'


def generated():
    return {THEME_JSON: theme_json(), PALETTE_RS: palette_rs()}


def write_generated():
    for path, text in generated().items():
        open(path, 'w').write(text)
        print('wrote', os.path.relpath(path, ROOT))


def check_generated():
    stale = [os.path.relpath(p, ROOT) for p, text in generated().items()
             if not os.path.exists(p) or open(p).read() != text]
    if stale:
        sys.exit('out of date (run palette.py --theme-json): ' + ', '.join(stale))
    print('The app’s theme and brand table match color.css.')


if __name__ == '__main__':
    unknown = [arg for arg in sys.argv[1:] if arg not in ('--write', '--theme-json', '--check-generated', '--all')]
    if unknown:
        sys.exit(f'unknown option {unknown[0]}; see the usage at the top of scripts/palette.py')
    if '--write' in sys.argv:
        write()
    if '--theme-json' in sys.argv:
        write_generated()
    if '--check-generated' in sys.argv:
        check_generated()
    check('--all' in sys.argv)
