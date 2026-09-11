# Changelog

## 0.1.0 — unreleased

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
  ext4 only so far.

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
