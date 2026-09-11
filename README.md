# spoor

Instant file-name search for Linux, in the spirit of Everything. A root daemon
keeps an index of a directory tree current through fanotify; a CLI, a GTK
window and a KDE KRunner plugin query it over a Unix socket.

> **Status: early.** Read [Security](#security) before installing on a machine
> that has more than one user.

## Why

Unprivileged indexers such as FSearch must place a fanotify or inotify mark on
every directory: on a home directory with ~240k folders that is ~240k marks, a
16,384-event queue that cannot be enlarged, and new directory trees that are
only seen if their marks get added in time. `FAN_MARK_FILESYSTEM`,
`FAN_UNLIMITED_QUEUE` and `FAN_UNLIMITED_MARKS` remove all three problems, but
they need `CAP_SYS_ADMIN` -- so spoor runs as a root daemon with a single mark
for the whole filesystem.

## Design

* The index lives in the daemon, so closing a window cannot invalidate it.
* Snapshots are written periodically and atomically, not only on exit, so a
  crash costs at most one interval. At start-up the snapshot is served at once
  while a full reconciliation walk runs behind it: ext4 keeps no change journal,
  so only a rescan can establish what changed while the daemon was down.
* Names are indexed by trigrams, with delta+varint posting lists and skip
  pointers. Path queries use trigrams on the last path segment and verify
  candidates against their reconstructed paths; with no usable trigram, a KMP
  automaton runs down the tree so that no path is ever materialised.
* Mounts that fanotify cannot see (rclone/FUSE, network filesystems) can be
  walked on a timer instead.

## Measured

On a laptop home directory of 2.3M files and 244k folders (ext4, 413 GB):

| | |
|---|---|
| full walk | 7-17 s (`updatedb`: 64 s) |
| trigram index build | ~5 s |
| one-word query, server-side | 0.1-0.7 ms |
| path or multi-word query | 1-5 ms |
| resident memory | ~340 MB |
| SIGKILL to a usable index | ~1 s |

Measure with `spoor bench <pattern>`. Timing `spoor query` mostly measures
process start-up, since the binary links GTK.

## Requirements

* Linux 5.9 or newer (`FAN_REPORT_DFID_NAME`), systemd, root.
* **Tested on ext4 only.** On btrfs or xfs the initial index works, but live
  updates are unverified and may silently stop applying.
* A recent stable Rust (developed with 1.93) and the GTK 3 development files.
  The KRunner plugin needs a KDE Plasma 6 session.

## Install

    make build
    sudo make install              # binary, systemd unit, desktop entry
    sudo systemctl enable --now spoor
    make install-krunner           # optional, per user, needs no root

The daemon indexes `/home`. `sudo make uninstall` removes it again.

## Searching

    spoor query invoice                  # names containing "invoice"
    spoor query wedding jpg              # both words, anywhere in the path
    spoor query '"annual report"'        # a phrase, space included
    spoor query --path projects/2024     # match against the full path
    spoor query --regex '^IMG_[0-9]+\.jpg$'
    spoor query --case README --files

* **One word** matches file names. **Several words** are ANDed, and each may
  match anywhere in the full path -- the name or any folder above it.
  **Quotes** keep a phrase together.
* A `/` in the query matches against the path; a trailing `/` (`photos/`)
  means "inside that folder".
* **Regex** mode treats the whole query as one expression (the `regex` crate:
  linear time, size-capped, since any local user can send patterns).

| option | GUI | CLI |
|---|---|---|
| Match Case | Ctrl+I | `--case` |
| Enable Regex | Ctrl+R | `--regex` |
| Search in Path | Ctrl+U | `--path` |
| Files only / Folders only | Search menu | `--files` / `--folders` |
| Hide dotfiles | Preferences | `--no-hidden` |

The GUI (`spoor gui`, or "File Search" in the application menu) keeps these in
`~/.config/spoor/gui.conf`. Double-click or Enter opens a result; right-click
offers Open With, Open Containing Folder, Copy Path, Copy Name, Move to Trash
and Properties (the file manager's own dialog, via
`org.freedesktop.FileManager1`). With the KRunner plugin, Alt+Space finds files
from anywhere.

## Network and FUSE mounts

fanotify only sees changes that pass through the local kernel, and the walk
stays on one filesystem, so a mount such as an rclone Google Drive is not
indexed by default. `--rescan PATH` (repeatable) walks it every
`--rescan-interval` seconds (default 900) instead. A path that is not currently
mounted is never walked -- it would read as an empty folder -- and is retried
every minute. Put machine-specific flags in a drop-in:

    # /etc/systemd/system/spoor.service.d/rescan.conf
    [Service]
    Environment="SPOOR_EXTRA_ARGS=--rescan /home/you/GoogleDrive"

A FUSE mount is private to its owner, so root cannot read it by default.
rclone accepts `--allow-root` but silently ignores it (its FUSE library dropped
support). What works is `--allow-other --default-permissions --umask 077`: the
kernel then enforces owner-only file modes, so other accounts -- service
accounts included -- are refused while root reads through its capabilities.
Plain `--allow-other` would expose the mount to every local account. Both need
`user_allow_other` in `/etc/fuse.conf`; confirm with `findmnt` that the kernel
options show `allow_other,default_permissions`.

The first walk after rclone starts goes to the provider's API and may be
throttled heavily (Google Drive with rclone's shared client ID: 8-25 entries/s).
Results are merged only when a walk completes.

## Security

The daemon runs as root and can list every name under the indexed tree.

* **Any local account can currently query it and see every file name** under
  the indexed root, including other users' homes: the socket is world-writable
  and results are not yet filtered by the caller's permissions. Until that is
  fixed, install spoor only on single-user machines.
* The snapshot under `/var/lib/spoor` is readable by root only.
* The socket serves at most 32 clients at once, with read and write timeouts
  and a 64 KiB request limit. Regex patterns cannot backtrack exponentially.
* Kernel event records are parsed with explicit bounds checks and unaligned
  reads. A panic aborts the process, and systemd restarts it, rather than
  leaving a poisoned lock behind a daemon that still looks healthy.

## Socket protocol

Line-based, on `/run/spoor.sock`:

    QUERY <limit> <pattern>            -> one path per line, blank line ends
    SEARCH <limit> <flags> <pattern>   -> same with options: c r p f d h, or -
    STATS                              -> "entries <n> arena <n>"

Errors come back as a line `ERR <message>`; paths always start with `/`.

## Known gaps

* Results are not filtered by the caller's permissions (see Security).
* btrfs and xfs: live updates unverified.
* A single indexed root.
* The arena is only compacted on restart; dead entries accumulate while running.
* File names containing a newline break the line protocol, and names that are
  not valid UTF-8 are stored lossily.
* Case-insensitive matching is ASCII-only.
* A multi-word query whose words are all shorter than 3 characters falls back
  to a full scan (up to ~0.9 s on 2.3M files when matches are rare).
* The GUI's columns do not sort, and a slow query briefly blocks the window.

## License

Licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT license
([LICENSE-MIT](LICENSE-MIT)), at your option.
