# Changelog

## 0.1.1 — 2026-09-18

### Command line

* `spoor query ... -exec command {} ;` and `-exec command {} +` run a command
  on the results, as find does, without a shell in the way. Every match is
  acted on, not the first hundred, and the exit status reports a command that
  failed.

### Window

* Shift-click and Ctrl-click select several results at once, and every action —
  open, open with, containing folder, copy, trash, properties — applies to the
  whole selection. Opening more than ten files at once asks first.
* Ctrl+C and Ctrl+X put the files themselves on the clipboard, for a file
  manager to paste as a copy or a move.
* Delete moves the selection to the trash; Shift+Delete deletes it for good,
  after a warning that names what is going.

### Documentation

* A benchmark anyone can run (`bench/`), measuring spoor against plocate,
  FSearch, find, fd and bfs, and the cost of watching a disk with fanotify,
  inotify or eBPF. The README leads with the result.

## 0.1.0 — 2026-09-14

First public release.

### Daemon

* One `FAN_MARK_FILESYSTEM` fanotify mark with an unlimited queue keeps the
  index current; new files are searchable within milliseconds.
* Trigram index with delta+varint posting lists and skip pointers; path
  queries anchor on the last path segment, with a KMP automaton run down the
  tree when no trigram applies.
* Atomic periodic snapshots (root-only) and a reconciliation walk at start-up
  and daily, which also catches changes made while the daemon was down.
* Any set of folders, each watched with its own fanotify mark, plus excluded
  folders and network folders. They are chosen in Preferences, stored in
  `/etc/spoor/spoor.conf` and applied through polkit.
* Network folders (rclone, network filesystems), which fanotify cannot see,
  are walked on a timer, and never while they are unmounted.
* Every reply is filtered by the caller's own permissions, checked by the
  kernel under the caller's credentials.
* File names are kept as raw bytes, so names that are not UTF-8, or that
  contain a newline, are found and open correctly.
* Case-insensitive matching folds Unicode, not only ASCII.
* File handles from ext4, btrfs (including subvolumes) and xfs are decoded;
  anything else falls back to `open_by_handle_at`. Live updates are tested on
  ext4, btrfs and xfs. A btrfs subvolume is watched through its containing
  mount, since it cannot carry a filesystem mark itself.

### Front ends

* `spoor query`: several words are ANDed across the full path; quotes keep a
  phrase; `--case`, `--regex`, `--path`, `--files`, `--folders`,
  `--no-hidden`, and `--null` for NUL-separated output.
* GTK window with FSearch's menus and shortcuts, a right-click menu (Open
  With, Open Containing Folder, Copy, Move to Trash, Properties), sortable
  columns, and searches that never block the window.
* KDE KRunner plugin.

### Packaging

* systemd unit with a hardened sandbox, a Debian package (`make deb`), and CI
  running rustfmt, clippy, the tests and a root self-test.
