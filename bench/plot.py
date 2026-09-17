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

# Colour follows the tool, and the three tools that walk the disk share one:
# find, fd and bfs do the same thing at different speeds. The order rows are
# drawn in is the one the palette validates for neighbouring marks.
THEMES = {
    "light": {
        "surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e", "muted": "#898781",
        "grid": "#e1e0d9", "axis": "#c3c2b7", "flat": "#c3c2b7",
        "spoor": "#2a78d6", "plocate": "#eb6834", "fsearch": "#1baf7a",
        "walk": "#eda100", "inotify": "#e87ba4", "ebpf": "#008300",
    },
    "dark": {
        "surface": "#1a1a19", "ink": "#ffffff", "ink2": "#c3c2b7", "muted": "#898781",
        "grid": "#2c2c2a", "axis": "#383835", "flat": "#4a4a46",
        "spoor": "#3987e5", "plocate": "#d95926", "fsearch": "#199e70",
        "walk": "#c98500", "inotify": "#d55181", "ebpf": "#008300",
    },
}

DAY = 86_400.0
CAP = 100_000  # bench.py stops every tool here
TICKS = [(1e-5, "10 µs"), (1e-4, "0.1 ms"), (1e-3, "1 ms"), (1e-2, "10 ms"),
         (1e-1, "100 ms"), (1, "1 s"), (10, "10 s"), (60, "1 min"),
         (600, "10 min"), (3600, "1 h"), (DAY, "1 day")]


def fmt(s):
    if s >= DAY:
        return f"{s / DAY:.0f} days"
    if s >= 3600:
        return f"{s / 3600:.0f} h"
    if s >= 1:
        return f"{s:.2g} s"
    if s >= 1e-3:
        return f"{s * 1e3:.3g} ms"
    return f"{s * 1e6:.0f} µs"


def median(values):
    return statistics.median(v for v in values if v is not None)


def style(ax, t, lo=None, hi=None, log=True, labels=True):
    ax.set_facecolor(t["surface"])
    if log:
        ax.set_xscale("log")
        ax.set_xlim(lo, hi)
        ticks = [v for v, _ in TICKS if lo <= v <= hi]
        ax.xaxis.set_major_locator(FixedLocator(ticks))
        ax.set_xticklabels([l for v, l in TICKS if lo <= v <= hi] if labels else [])
        ax.xaxis.set_minor_locator(NullLocator())
    ax.grid(axis="x", color=t["grid"], linewidth=1)
    ax.set_axisbelow(True)
    for side in ("top", "right", "bottom"):
        ax.spines[side].set_visible(False)
    ax.spines["left"].set_color(t["axis"])
    ax.tick_params(axis="x", colors=t["muted"], length=0, labelsize=8.5)
    ax.tick_params(axis="y", colors=t["ink2"], length=0, labelsize=9.5)


def heading(ax, t, head, sub=None, size=12.5):
    ax.set_title(head, loc="left", color=t["ink"], fontsize=size,
                 fontweight="semibold", pad=24 if sub else 10)
    if sub:
        ax.text(0, 1.02, sub, transform=ax.transAxes, color=t["ink2"],
                fontsize=9, va="bottom")


def bars(ax, t, rows, height=0.62):
    """rows: (label, value, colour key, note), or a fifth field marking a bar
    whose length is a policy rather than a measurement: those are drawn hollow.
    Values are labelled in ink past the bar's end, never in the bar's colour."""
    lo, hi = ax.get_xlim()
    log = ax.get_xscale() == "log"
    for i, row in enumerate(rows):
        _, v, key, note = row[:4]
        left = lo if log else 0
        ax.barh(i, v - left, height=height, left=left,
                color="none" if row[4:] else t[key],
                edgecolor=t[key], linewidth=1.2 if row[4:] else 0,
                hatch="////" if row[4:] else None)
        ax.annotate(note, (v, i), xytext=(5, 0), textcoords="offset points",
                    va="center", color=t["ink2"], fontsize=8.5)
    ax.set_yticks(range(len(rows)), [r[0] for r in rows])
    ax.set_ylim(len(rows) - 0.5, -0.5)
    ax.set_xlim(lo, hi)  # barh autoscales; keep the axis as styled


def draw_detail(r, theme):
    t = THEMES[theme]
    plt.rcParams["font.family"] = ["DejaVu Sans"]
    fig = plt.figure(figsize=(12.5, 15.5), facecolor=t["surface"])
    outer = fig.add_gridspec(4, 1, height_ratios=[3.05, 0.95, 0.85, 1.0], hspace=0.42,
                             left=0.175, right=0.985, top=0.915, bottom=0.03)
    m, b = r["machine"], r["build"]
    fig.text(0.02, 0.975, "Finding a file on Linux: spoor vs. the alternatives",
             color=t["ink"], fontsize=18, fontweight="bold")
    fig.text(0.02, 0.957,
             f"{b['entries']:,} files and folders on an {m['filesystem']} disk · "
             f"{m['cpu']} · Linux {m['kernel']} · {m['spoor']}, {m['plocate']}"
             + (f", {m['fsearch']}" if m.get("fsearch") else ""),
             color=t["muted"], fontsize=9)

    # ---------------------------------------------------------- search
    fsearch = (r.get("fsearch") or {}).get("search_s", {})
    grid = outer[0].subgridspec(3, 2, hspace=0.8, wspace=0.62)
    for i, q in enumerate(r["search"]):
        ax = fig.add_subplot(grid[i // 2, i % 2])
        style(ax, t, 1e-4, 3000)
        rows = [("spoor, from the window", q["spoor_socket_s"], "spoor", None),
                ("spoor query (command)", q["spoor_s"], "spoor", None)]
        if q["pattern"] in fsearch:
            rows.append(("FSearch (in its window)", fsearch[q["pattern"]], "fsearch", None))
        rows.append(("plocate", q["plocate_s"], "plocate", None))
        if "fd_s" in q:
            rows.append(("fd", q["fd_s"], "walk", None))
        rows += [("bfs", q["bfs_s"], "walk", None), ("find", q["find_s"], "walk", None)]
        rows = [(lab, v, key, fmt(v)) for lab, v, key, _ in rows]
        bars(ax, t, rows, height=0.6)
        hits = q["plocate_hits"]
        count = f"first {hits:,}" if hits >= CAP else f"{hits:,} match{'es' * (hits != 1)}"
        heading(ax, t, q["label"], f"“{q['pattern']}” · {count}", size=11.5)
    notes = fig.add_subplot(grid[2, 1])
    notes.axis("off")
    notes.text(0, 1.0, "How this was measured", color=t["ink"], fontsize=11.5,
               fontweight="semibold", va="top")
    notes.text(0, 0.8,
               "Every tool lists the same matches: names\n"
               "containing the pattern, ignoring case, up to\n"
               "100,000. Median of 15 runs, warm cache,\n"
               "searching as an ordinary user.\n\n"
               "The commands include starting a process.\n"
               "spoor from the window and FSearch are timed\n"
               "inside a program already running, as they are\n"
               "used in practice; FSearch's own figure leaves\n"
               "out drawing the results.",
               color=t["ink2"], fontsize=8.5, va="top", linespacing=1.55)

    # ---------------------------------------------------------- freshness
    ax = fig.add_subplot(outer[1])
    style(ax, t, 1e-4, 700 * DAY)
    f = r["fresh"]
    rows = [("spoor", median(f["spoor_s"]), "spoor", fmt(median(f["spoor_s"])))]
    if "ebpf_s" in f:
        rows.append(("an eBPF watcher", median(f["ebpf_s"]), "ebpf",
                     f"{fmt(median(f['ebpf_s']))}, after "
                     f"{fmt(r['watch']['ebpf_setup_s'])} to start watching"))
    rows += [
        ("an inotify watcher", median(f["inotify_s"]), "inotify",
         f"{fmt(median(f['inotify_s']))}, after "
         f"{fmt(r['watch']['inotify_setup_s'])} to start watching"),
        ("find", f["find_s"], "walk", f"{fmt(f['find_s'])}: every search walks the disk"),
        ("FSearch", 900, "fsearch", "only when it rescans: at startup, or on a timer", 1),
        ("plocate", DAY, "plocate", "up to a day, until updatedb next runs", 1),
    ]
    bars(ax, t, rows)
    heading(ax, t, "A file you just saved shows up after",
            f"A new file in a new nested folder, median of {len(f['spoor_s'])} tries. "
            "Hollow bars are not measurements: that is how long the tool leaves a "
            "new file unfindable by design.")

    # ---------------------------------------------------------- overhead
    ax = fig.add_subplot(outer[2])
    o = r["overhead"]
    style(ax, t, log=False)
    base = o["nothing_s"]
    rows = [("nothing watching", base, "flat", f"{fmt(base)}")]
    for label, key, colour in (("one fanotify mark", "fanotify", "spoor"),
                               ("spoor (mark + indexing)", "spoor", "spoor"),
                               ("inotify, one watch per folder", "inotify", "inotify"),
                               ("an eBPF watcher", "ebpf", "ebpf")):
        if key + "_s" in o:
            v = o[key + "_s"]
            rows.append((label, v, colour, f"{fmt(v)}   +{(v / base - 1) * 100:.0f}%"))
    ax.set_xlim(0, max(v for _, v, _, _ in rows) * 1.35)
    bars(ax, t, rows)
    ax.set_xlabel("seconds", color=t["muted"], fontsize=8.5)
    heading(ax, t, "What watching costs everything else",
            f"{o['operations']:,} file operations — create, rename, delete — "
            "with each watcher running.")

    # ---------------------------------------------------------- startup
    ax = fig.add_subplot(outer[3])
    style(ax, t, 1e-5, 20000)
    w = r["watch"]
    rows = [("spoor: index the tree", b["spoor_s"], "spoor",
             f"{fmt(b['spoor_s'])}, {b['spoor_rss_mb']:.0f} MB in memory")]
    if "fsearch_s" in b:
        rows.append(("FSearch: index the tree", b["fsearch_s"], "fsearch",
                     f"{fmt(b['fsearch_s'])}, {b['fsearch_rss_mb']:.0f} MB in memory, "
                     f"{b['fsearch_db_mb']:.0f} MB on disk"))
    rows.append(("plocate: updatedb", b["updatedb_s"], "plocate",
                 f"{fmt(b['updatedb_s'])}, {b['plocate_db_mb']:.0f} MB on disk"))
    rows.append(("spoor: start watching", w["fanotify_setup_s"], "spoor",
                 f"{fmt(w['fanotify_setup_s'])}: 1 fanotify mark covers the whole disk"))
    if "ebpf_setup_s" in w:
        rows.append(("eBPF: start watching", w["ebpf_setup_s"], "ebpf",
                     f"{fmt(w['ebpf_setup_s'])}: {w['ebpf_marks']} programs loaded "
                     "into the kernel"))
    rows.append(("inotify: start watching", w["inotify_setup_s"], "inotify",
                 f"{fmt(w['inotify_setup_s'])}: {w['inotify_marks']:,} watches, one per folder "
                 f"(limit {w['max_user_watches']:,})"))
    bars(ax, t, rows)
    heading(ax, t, "Getting started",
            "Building an index from nothing, and being ready to hear about changes.")

    out = os.path.join(DOCS, f"benchmark-detail-{theme}.png")
    fig.savefig(out, dpi=110, facecolor=t["surface"])
    plt.close(fig)
    print(out)


def draw_simple(r, theme):
    """The README's headline: one selective search, on a plain linear scale.

    Linear is the honest reading of "how long do I wait", but it flattens
    everything under a second into the baseline, so every bar carries its own
    number and the caption says which search this is."""
    t = THEMES[theme]
    plt.rcParams["font.family"] = ["DejaVu Sans"]
    fig = plt.figure(figsize=(11, 5.4), facecolor=t["surface"])
    ax = fig.add_axes([0.2, 0.2, 0.775, 0.54])
    m, b = r["machine"], r["build"]
    q = r["search"][0]
    fsearch = (r.get("fsearch") or {}).get("search_s", {}).get(q["pattern"])

    fig.text(0.025, 0.93, "Finding one file among a million", color=t["ink"],
             fontsize=19, fontweight="bold")
    fig.text(0.025, 0.875,
             f"Time to find “{q['pattern']}” by name. {b['entries']:,} files and "
             f"folders on an {m['filesystem']} disk, {m['cpu'].split('@')[0].strip()}.",
             color=t["ink2"], fontsize=10.5)

    rows = [("spoor", q["spoor_socket_s"], "spoor")]
    if fsearch:
        rows.append(("FSearch", fsearch, "fsearch"))
    rows += [("plocate", q["plocate_s"], "plocate")]
    if "fd_s" in q:
        rows.append(("fd", q["fd_s"], "walk"))
    rows += [("bfs", q["bfs_s"], "walk"), ("find", q["find_s"], "walk")]
    rows.sort(key=lambda row: row[1])
    slowest = max(v for _, v, _ in rows)

    ax.set_facecolor(t["surface"])
    ax.set_xlim(0, slowest * 1.30)
    for i, (label, v, key) in enumerate(rows):
        ax.barh(i, v, height=0.62, color=t[key])
        note = f"{fmt(v)}      too fast to draw at this scale"
        if i:
            note = f"{fmt(v)}      {v / rows[0][1]:,.0f}× slower"
        ax.annotate(note, (v, i), xytext=(7, 0), textcoords="offset points",
                    va="center", color=t["ink2"], fontsize=10)
    ax.set_yticks(range(len(rows)), [r[0] for r in rows])
    ax.set_ylim(len(rows) - 0.5, -0.5)
    ax.grid(axis="x", color=t["grid"], linewidth=1)
    ax.set_axisbelow(True)
    for side in ("top", "right", "bottom"):
        ax.spines[side].set_visible(False)
    ax.spines["left"].set_color(t["axis"])
    ax.tick_params(axis="x", colors=t["muted"], length=0, labelsize=9.5)
    ax.tick_params(axis="y", colors=t["ink"], length=0, labelsize=12)
    ax.set_xlabel("seconds", color=t["muted"], fontsize=9.5, labelpad=6)

    fig.text(0.025, 0.035,
             "A search with few matches, where an index pays off most. With tens of "
             "thousands of matches the gap narrows,\nand FSearch or fd can come out "
             "ahead — bench/ has every measurement, including the ones spoor loses.",
             color=t["muted"], fontsize=9, linespacing=1.6)

    out = os.path.join(DOCS, f"benchmark-{theme}.png")
    fig.savefig(out, dpi=110, facecolor=t["surface"])
    plt.close(fig)
    print(out)


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.join(HERE, "results.json")
    r = json.load(open(path))
    for theme in THEMES:
        draw_simple(r, theme)
        draw_detail(r, theme)


if __name__ == "__main__":
    main()
