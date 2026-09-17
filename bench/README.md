# Benchmark

How spoor compares with the other ways to find a file on Linux, and with the
other ways to hear that a file has changed. Everything is measured on one
synthetic tree, so anyone can check the numbers.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="../docs/benchmark-dark.png">
  <img alt="Benchmark results" src="../docs/benchmark-light.png">
</picture>

## What is compared

Searching:

* **spoor, from the window**: one search request over spoor's socket, as the
  window and KRunner send it. No process to start.
* **`spoor query`**: the same search from the command line, including starting
  the `spoor` process.
* **FSearch**: the Linux take on Windows' Everything — its own index, held in
  memory by the running program.
* **plocate**: the fastest `locate`, with its database built by `updatedb`.
* **find**, **fd**, **bfs**: no index; every search walks the disk.

Hearing about changes:

* **fanotify**, what spoor uses: one mark covers a whole filesystem, including
  folders that do not exist yet. Needs root.
* **inotify** (`watchers.c`): what an indexer running without root has to do,
  with one watch on every folder. It is written carefully, not as a straw man:
  a folder created later is watched and listed at once, so files made inside it
  before the watch took hold are still reported.
* **eBPF** (`ebpf_watch.bpf.c`, `ebpf_watch.c`): programs loaded into the
  kernel on the security hooks that every create, rename and delete passes
  through, sending paths to user space through a ring buffer. This is how
  security tools such as Falco and Tetragon watch files. No file search tool
  uses it.

All searches ask for the same thing: every file or folder whose name contains
the pattern, ignoring case, up to 100,000 results. The harness checks that the
command-line tools return the same number of results. Searches run as your
user, with a warm disk cache; the figure shows the median of 15 runs after 2
warm-up runs.

Not included: **Baloo** and **Tracker/LocalSearch** index file contents too,
and choose their own folders, so their numbers would not measure the same work.

## Running it

It needs root (for fanotify, eBPF, `updatedb` and mounting the test image), a
release build of spoor, and a few packages:

    sudo apt install plocate bfs fd-find fsearch clang libbpf-dev bpftool xvfb
    make build
    sudo python3 bench/bench.py
    uv run --with matplotlib bench/plot.py

`bench.py` makes an ext4 image under `/var/tmp/spoor-bench`, mounts it, and
fills it with about a million empty files (about a minute the first time; later
runs reuse them), then writes `bench/results.json`. `plot.py` draws
`docs/benchmark-light.png` and `docs/benchmark-dark.png` from that file.
`--files`, `--runs`, `--dir` and `--no-fsearch` change the setup; `--help`
lists everything. Anything missing is skipped rather than fatal, except spoor
itself. To remove it all:

    sudo umount /var/tmp/spoor-bench/mnt && sudo rm -rf /var/tmp/spoor-bench

## Notes on fairness

* **The tree has its own filesystem.** An installed spoor service watches `/`
  with a filesystem-wide mark, so every file operation there already pays
  fanotify's cost. Measuring "nothing watching" on that disk would measure
  spoor instead of nothing.
* **FSearch has no command line.** Its window runs on a hidden X display, and
  `fsearch -s` hands it each search. The time comes from FSearch's own debug
  log, so it covers the search inside the program but not drawing the results —
  which flatters it against the numbers that include starting a process. Its
  result counts are not in that log, so unlike the other tools its matches are
  not checked against the rest.
* **The eBPF watcher is compiled ahead of time**, as a shipped tool would be,
  so its startup figure is loading, verifying and attaching — not compiling.
* **`spoor query` carries GTK.** One binary holds the daemon, the window and
  the command, so a command-line search pays about 20 ms to load GTK before it
  starts. That is a packaging problem, not an index one, and it is why the
  window's number and the command's number are both shown.

## Reading the results

Measured on 1,048,839 files and folders (ext4, Intel i7-1165G7, Linux 7.0):

* **A selective search is where an index pays off.** For one file by name,
  spoor answers in 0.35 ms from the window. FSearch takes 233 ms, because it
  compares every name in the tree on every search; plocate takes 17 ms; the
  three walkers take 1 to 3.6 seconds.
* **The more matches, the less the index helps.** spoor goes from 0.35 ms for
  one match to 109 ms for 12,473 and 386 ms for the first 100,000. FSearch is
  flat at about 220 ms whatever it is asked, so beyond roughly 50,000 matches
  it overtakes spoor — at that point the work is producing results, not finding
  them. `fd` walks the whole tree in parallel and stops at the cap, which also
  puts it ahead of spoor for the 100,000-match search (373 ms against 386 ms).
* **`spoor query` is slower than plocate when few files match** (34 ms against
  17 ms for one file). The search itself takes 0.35 ms; the rest is starting a
  binary that loads GTK.
* **Two letters** is spoor's documented slow case, because a search that short
  cannot use the index and every name must be looked at: 154 ms, still ahead of
  plocate's 503 ms and FSearch's 209 ms.
* **New files.** spoor lists a file in a new nested folder about 1 ms after it
  is created. plocate cannot find it until `updatedb` runs, which Debian and
  Ubuntu do once a day; FSearch until it rescans, which by default happens only
  when it starts.
* **Watching costs about the same whichever mechanism you pick**: over 122,000
  file operations, one fanotify mark cost 17%, an eBPF watcher 18% and inotify
  20%. spoor itself cost 38%, because it also keeps its index up to date; that
  is the price of a search being instant afterwards.
* **The difference is in what it takes to be ready.** fanotify: one mark, 27 µs,
  the whole filesystem covered. eBPF: 8 programs, 220 ms. inotify: 48,958
  watches, one per folder, 2.3 s, and a system-wide limit of 256,591 watches on
  this machine — a large home folder can exhaust it, and each watch costs
  kernel memory.
* **No watcher missed anything.** In a burst of 2,000 new nested folders, all
  four saw every file, and the eBPF ring buffer dropped no events. inotify
  keeps up because the watcher lists each new folder as soon as it watches it;
  a watcher that skips that step loses files.
* **Cost of the indexes.** spoor keeps 133 MB in memory and builds in 4.9 s.
  FSearch keeps 226 MB and builds in 15 s (46 MB on disk). plocate's database is
  26 MB on disk and `updatedb` takes 8.3 s.
* The tree is synthetic: empty files with names built from common words. Your
  own files will give different numbers, but the comparison should hold.
