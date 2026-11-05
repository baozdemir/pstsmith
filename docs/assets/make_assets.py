"""Writes pstsmith's wordmark (light and dark) and the repository's social preview page.

    python3 docs/assets/make_assets.py
    chrome --headless=new --hide-scrollbars --allow-file-access-from-files --window-size=1280,640 \
        --screenshot=docs/assets/social-preview.png docs/assets/social-preview.html     # the 1280x640 social preview

The letters are monoline strokes drawn as SVG paths, so the wordmark looks the same wherever it is shown, without a
font. The mark is an envelope whose flap carries the temper colors of forged steel (straw, bronze, purple, blue).
"""
from pathlib import Path

HERE = Path(__file__).resolve().parent
TEMPER = [("0", "#D9B44A"), ("0.35", "#B06A2C"), ("0.68", "#6E3C82"), ("1", "#2C5EA8")]
STROKE = 7


def letters(x0):
    """The paths of "pstsmith" from x0, baseline 60, x-height 20..60, and the end x."""
    paths, x = [], x0
    # p
    paths.append(f"M{x} 22V84 M{x} 40A17 18 0 1 1 {x} 41")
    x += 46
    # s
    paths.append(f"M{x + 24} 26C{x + 20} 20 {x + 2} 19 {x + 1} 31C{x} 42 {x + 25} 37 {x + 25} 50C{x + 24} 62 {x + 4} 61 {x} 54")
    x += 38
    # t
    paths.append(f"M{x + 6} 6V50C{x + 6} 58 {x + 12} 61 {x + 20} 59 M{x - 3} 24H{x + 19}")
    x += 33
    # s
    paths.append(f"M{x + 24} 26C{x + 20} 20 {x + 2} 19 {x + 1} 31C{x} 42 {x + 25} 37 {x + 25} 50C{x + 24} 62 {x + 4} 61 {x} 54")
    x += 39
    # m
    paths.append(f"M{x} 60V22 M{x} 34C{x} 18 {x + 20} 18 {x + 20} 34V60 M{x + 20} 34C{x + 20} 18 {x + 40} 18 {x + 40} 34V60")
    x += 54
    # i
    paths.append(f"M{x} 24V60")
    dot = (x, 8)
    x += 15
    # t
    paths.append(f"M{x + 6} 6V50C{x + 6} 58 {x + 12} 61 {x + 20} 59 M{x - 3} 24H{x + 19}")
    x += 34
    # h
    paths.append(f"M{x} 0V60 M{x} 36C{x} 16 {x + 24} 16 {x + 24} 36V60")
    x += 24
    return paths, dot, x


def wordmark(ink, file):
    mark_w = 92
    paths, dot, end = letters(mark_w + 26)
    width, height = end + 8, 92
    svg = f'''<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="-4 -4 {width + 4} {height}" role="img" aria-label="pstsmith">
  <defs>
    <linearGradient id="temper" x1="0" y1="0" x2="1" y2="0">
      {"".join(f'<stop offset="{o}" stop-color="{c}"/>' for o, c in TEMPER)}
    </linearGradient>
  </defs>
  <g fill="none" stroke-linecap="round" stroke-linejoin="round" stroke-width="{STROKE}">
    <rect x="2" y="10" width="{mark_w - 6}" height="62" rx="9" stroke="{ink}"/>
    <path d="M10 20L{mark_w / 2 - 1} 47L{mark_w - 12} 20" stroke="url(#temper)" stroke-width="{STROKE + 2}"/>
    <path d="{' '.join(paths)}" stroke="{ink}"/>
  </g>
  <circle cx="{dot[0]}" cy="{dot[1]}" r="{STROKE * 0.75}" fill="{ink}"/>
</svg>
'''
    (HERE / file).write_text(svg)


if __name__ == "__main__":
    wordmark("#1C2733", "wordmark-light.svg")
    wordmark("#E6EBF0", "wordmark-dark.svg")
