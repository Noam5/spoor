#!/usr/bin/env python3
"""Draw bench/results.json as the README's benchmark figure.

  uv run --with matplotlib bench/plot.py [results.json]

Writes docs/benchmark-light.png and docs/benchmark-dark.png; the README picks
one by the reader's theme.
"""

import json
import os
import statistics
import sys

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib.ticker import FixedLocator, NullLocator  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
DOCS = os.path.join(os.path.dirname(HERE), "docs")

THEMES = {
    "light": {
        "surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e", "muted": "#898781",
        "grid": "#e1e0d9", "axis": "#c3c2b7",
        "series": ["#2a78d6", "#eb6834", "#1baf7a", "#eda100", "#e87ba4"],
    },
    "dark": {
        "surface": "#1a1a19", "ink": "#ffffff", "ink2": "#c3c2b7", "muted": "#898781",
        "grid": "#2c2c2a", "axis": "#383835",
        "series": ["#3987e5", "#d95926", "#199e70", "#c98500", "#d55181"],
    },
}
# Color follows the tool in every panel.
SPOOR, SPOOR_CLI, PLOCATE, FIND, INOTIFY = range(5)

DAY = 86_400.0
CAP = 100_000  # bench.py stops every tool here
TICKS = [(1e-5, "10 µs"), (1e-4, "0.1 ms"), (1e-3, "1 ms"), (1e-2, "10 ms"),
         (1e-1, "100 ms"), (1, "1 s"), (10, "10 s"), (60, "1 min"),
         (600, "10 min"), (3600, "1 h"), (DAY, "1 day")]


def fmt(s):
    if s >= 3600:
        return f"{s / 3600:.0f} h"
    if s >= 1:
        return f"{s:.1f} s"
    if s >= 1e-3:
        return f"{s * 1e3:.3g} ms"
    return f"{s * 1e6:.0f} µs"


def style_axis(ax, t, lo, hi):
    ax.set_facecolor(t["surface"])
    ax.set_xscale("log")
    ax.set_xlim(lo, hi)
    ticks = [v for v, _ in TICKS if lo <= v <= hi]
    ax.xaxis.set_major_locator(FixedLocator(ticks))
    ax.set_xticklabels([l for v, l in TICKS if lo <= v <= hi])
    ax.xaxis.set_minor_locator(NullLocator())
    ax.grid(axis="x", color=t["grid"], linewidth=1)
    ax.set_axisbelow(True)
    for side in ("top", "right", "bottom"):
        ax.spines[side].set_visible(False)
    ax.spines["left"].set_color(t["axis"])
    ax.tick_params(axis="x", colors=t["muted"], length=0, labelsize=9)
    ax.tick_params(axis="y", colors=t["ink2"], length=0, labelsize=10)


def title(ax, t, head, sub):
    ax.set_title(head, loc="left", color=t["ink"], fontsize=13, fontweight="semibold", pad=26)
    ax.text(0, 1.02, sub, transform=ax.transAxes, color=t["ink2"], fontsize=9.5, va="bottom")


def bars(ax, t, rows, height=0.62):
    """rows: (y, value, color index, label text). Labels sit past the bar tip,
    in ink, never in the bar's color."""
    lo, hi = ax.get_xlim()
    for y, v, c, text in rows:
        # On a log axis a bar needs a finite start: grow it from the left edge.
        ax.barh(y, v - lo, height=height, color=t["series"][c], left=lo)
        ax.annotate(text, (v, y), xytext=(5, 0), textcoords="offset points",
                    va="center", color=t["ink2"], fontsize=8.5)
    ax.set_xlim(lo, hi)  # barh autoscales; keep the axis as styled


def draw(r, theme):
    t = THEMES[theme]
    plt.rcParams["font.family"] = ["DejaVu Sans"]
    fig = plt.figure(figsize=(11, 11.5), facecolor=t["surface"])
    gs = fig.add_gridspec(3, 1, height_ratios=[5.2, 2.4, 2.2], hspace=0.55,
                          left=0.24, right=0.96, top=0.86, bottom=0.05)
    m = r["machine"]
    fig.text(0.03, 0.96, "spoor vs. plocate, find and inotify", color=t["ink"],
             fontsize=17, fontweight="bold")
    fig.text(0.03, 0.935,
             f"{r['build']['entries']:,} files and folders · {m['filesystem']} · "
             f"{m['cpu']} · Linux {m['kernel']}",
             color=t["muted"], fontsize=9.5)

    # --- search
    ax = fig.add_subplot(gs[0])
    style_axis(ax, t, 1e-4, 1000)
    series = [("spoor, from the window", "spoor_socket_s", SPOOR),
              ("spoor query (command)", "spoor_s", SPOOR_CLI),
              ("plocate", "plocate_s", PLOCATE),
              ("find", "find_s", FIND)]
    band, h = 1.0, 0.19
    rows, labels = [], []
    for i, q in enumerate(r["search"]):
        y0 = i * band
        for j, (_, key, c) in enumerate(series):
            rows.append((y0 + (j - 1.5) * (h + 0.02), q[key], c, fmt(q[key])))
        hits = q["plocate_hits"]
        count = f"first {hits:,}" if hits >= CAP else f"{hits:,} match{'es' * (hits != 1)}"
        labels.append(f"{q['label']}\n“{q['pattern']}” · {count}")
    bars(ax, t, rows, height=h)
    ax.set_yticks([i * band for i in range(len(r["search"]))], labels)
    ax.invert_yaxis()
    title(ax, t, "Search time",
          "Median time to list every match (at most 100,000). The commands include starting a process.")
    handles = [plt.Rectangle((0, 0), 1, 1, color=t["series"][c]) for _, _, c in series]
    ax.legend(handles, [n for n, _, _ in series], loc="center right", frameon=False,
              labelcolor=t["ink2"], fontsize=9.5)

    # --- freshness
    ax = fig.add_subplot(gs[1])
    style_axis(ax, t, 1e-4, 40 * DAY)
    f = r["fresh"]
    med = lambda v: statistics.median(x for x in v if x is not None)  # noqa: E731
    rows = [
        (0, med(f["spoor_s"]), SPOOR, fmt(med(f["spoor_s"]))),
        (1, med(f["inotify_s"]), INOTIFY,
         f"{fmt(med(f['inotify_s']))}, after {fmt(r['watch']['inotify_setup_s'])} "
         "to start watching"),
        (2, f["find_s"], FIND, f"{fmt(f['find_s'])}: every search walks the disk"),
        (3, DAY, PLOCATE, "up to 1 day"),
    ]
    bars(ax, t, rows)
    ax.set_yticks(range(4), ["spoor", "an inotify watcher", "find", "plocate"])
    ax.invert_yaxis()
    title(ax, t, "A file you just saved shows up after",
          "A new file in a new nested folder; median of "
          f"{len(f['spoor_s'])} tries. plocate waits for updatedb, which runs daily.")

    # --- startup
    ax = fig.add_subplot(gs[2])
    style_axis(ax, t, 1e-5, 600)
    w, b = r["watch"], r["build"]
    rows = [
        (0, b["spoor_s"], SPOOR, f"{fmt(b['spoor_s'])}, {b['spoor_rss_mb']:.0f} MB in memory"),
        (1, b["updatedb_s"], PLOCATE, f"{fmt(b['updatedb_s'])}, {b['plocate_db_mb']:.0f} MB on disk"),
        (2.4, w["fanotify_setup_s"], SPOOR, f"{fmt(w['fanotify_setup_s'])}: 1 mark covers the disk"),
        (3.4, w["inotify_setup_s"], INOTIFY,
         f"{fmt(w['inotify_setup_s'])}: {w['inotify_watches']:,} watches, one per folder"),
    ]
    bars(ax, t, rows)
    ax.set_yticks([0, 1, 2.4, 3.4], ["spoor: index", "plocate: updatedb",
                                     "spoor: fanotify", "inotify watcher"])
    ax.invert_yaxis()
    title(ax, t, "Getting started", "Indexing from nothing, and watching every folder for changes.")

    out = os.path.join(DOCS, f"benchmark-{theme}.png")
    fig.savefig(out, dpi=110, facecolor=t["surface"])
    plt.close(fig)
    print(out)


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "results.json")
    r = json.load(open(path))
    for theme in THEMES:
        draw(r, theme)


if __name__ == "__main__":
    main()
