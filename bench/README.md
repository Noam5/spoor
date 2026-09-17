# Benchmark

How spoor compares with the other ways to find a file on Linux, measured on
one synthetic tree so anyone can check the numbers.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../docs/benchmark-dark.png">
  <img alt="Benchmark results" src="../docs/benchmark-light.png">
</picture>

## What is compared

* **spoor, from the window**: one search request over spoor's socket, as the
  window and KRunner send it. No process start.
* **`spoor query`**: the same search from the command line, including starting
  the `spoor` process.
* **plocate**: the fastest `locate`, with its database built by `updatedb`.
* **find**: no index; every search walks the disk.
* **An inotify watcher** (`watchers.c`): what an indexer running without root
  has to do to hear about changes, with one watch on every folder. It is
  written carefully: a folder created later is listed as soon as it is watched,
  so files made inside it before then are not missed.

All searches ask for the same thing: every file or folder whose name contains
the pattern, ignoring case. The harness checks that every tool returns the same
number of results. Searches run as your user, with a warm disk cache, and the
figure shows the median of 15 runs after 2 warm-up runs.

Not included:

* **FSearch** has no command line; it only searches from its window.
* **eBPF**: no file search tool uses it to track changes. It is suited to
  tracing, and would need kernel-version-specific hooks to work out full paths.
* **Baloo and Tracker/LocalSearch** index file contents too, and choose
  their own folders, so their numbers would not measure the same work.

## Running it

It needs root for spoor's service (fanotify) and for `updatedb`, a
release build, `plocate`, and a C compiler:

    make build
    sudo python3 bench/bench.py
    uv run --with matplotlib bench/plot.py

`bench.py` creates about a million empty files under `/var/tmp/spoor-bench`
(about 45 seconds the first time; later runs reuse them), then writes
`bench/results.json`. `plot.py` draws `docs/benchmark-light.png` and
`docs/benchmark-dark.png` from that file. `--files`, `--runs` and `--dir`
change the setup; `--help` lists everything. Remove the tree with
`sudo rm -rf /var/tmp/spoor-bench`.

## Reading the results

Measured on 1,048,839 files and folders (ext4, Intel i7-1165G7, Linux 7.0):

* **Searching.** From the window, spoor is the fastest for every query. How
  long a search takes depends mostly on how many matches come back: 0.2 ms for
  one file, 80 ms for 12,000, 250 ms for 100,000. plocate takes 1.5 to 6 times
  as long; find walks the whole tree every time, about 2.5 s.
* **`spoor query` is slower than plocate when there are few matches** (24 ms
  against 8 ms for one file). The search itself takes 0.2 ms; the rest is
  starting `spoor`, which loads GTK even for a command-line search.
* **Two letters** is spoor's slow case, because a two-letter search cannot use
  the index and must scan every name: 105 ms, still ahead of plocate's 229 ms.
* **New files.** spoor lists a file in a new nested folder within half a
  millisecond. plocate cannot find it until `updatedb` runs again, which Debian
  and Ubuntu do once a day.
* **inotify vs. fanotify.** Once running, an inotify watcher hears about new
  files as fast as spoor does, and in a burst of 2,000 new nested folders both
  saw every file. The cost is up front: it needs 48,807 watches, one per
  folder, and 1.5 s to add them, and it stops at
  `fs.inotify.max_user_watches` (256,591 here). spoor needs one fanotify mark
  for the whole disk, set in 23 µs.
* **Cost.** spoor keeps its index in memory: 133 MB for this tree. plocate's
  database is 26 MB on disk. spoor indexes the tree from nothing in 3.5 s;
  `updatedb` takes 6.1 s.
* The tree is synthetic: empty files with names built from common words. The
  numbers on your own files will differ, but the comparison should not.
