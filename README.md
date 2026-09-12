# spoor

Instant file-name search for Linux, in the spirit of Everything. A root daemon
keeps an index of a directory tree current through fanotify; a CLI, a GTK
window and a KDE KRunner plugin query it over a Unix socket.

> **Status: 0.1.** Tested on ext4; see [Requirements](#requirements) for
> btrfs and xfs.

## Why

Unprivileged indexers such as FSearch must place a fanotify or inotify mark on
every directory: on a home directory with ~240k folders that is ~240k marks, a
16,384-event queue that cannot be enlarged, and new directory trees that are
only seen if their marks get added in time. `FAN_MARK_FILESYSTEM`,
`FAN_UNLIMITED_QUEUE` and `FAN_UNLIMITED_MARKS` remove all three problems, but
they need `CAP_SYS_ADMIN` -- so spoor runs as a root daemon with a single mark
for the whole filesystem, and checks every answer against the asking user's
own permissions.

## Design

* The index lives in the daemon, so closing a window cannot invalidate it.
* Snapshots are written periodically and atomically, not only on exit, so a
  crash costs at most one interval. At start-up the snapshot is served at once
  while a full reconciliation walk runs behind it: ext4 keeps no change journal,
  so only a rescan can establish what changed while the daemon was down. The
  walk repeats daily (`--reconcile-interval`).
* Names are indexed by trigrams, with delta+varint posting lists and skip
  pointers. Path queries use trigrams on the last path segment and verify
  candidates against their reconstructed paths; with no usable trigram, a KMP
  automaton runs down the tree so that no path is ever materialised.
* Names are raw bytes throughout, as Linux stores them: a name that is not
  UTF-8, or that contains a newline, is found and opens correctly.
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
| regex (a scan of every name) | 50-250 ms |
| resident memory | ~340 MB |
| SIGKILL to a usable index | ~1 s |

Measure with `spoor bench <pattern>`. Timing `spoor query` mostly measures
process start-up, since the binary links GTK.

## Requirements

* Linux 5.9 or newer (`FAN_REPORT_DFID_NAME`), systemd, root.
* **Live updates are tested on ext4, btrfs and xfs.** A btrfs subvolume
  cannot carry a filesystem mark of its own, so spoor marks its containing
  mount and keeps that subtree's events. Other filesystems fall back to
  `open_by_handle_at`; where live updates do not arrive, the daily
  reconciliation walk bounds the drift.
* Rust 1.87 or newer (zbus requires it) and the GTK 3 development files
  (`libgtk-3-dev`). The
  KRunner plugin needs a KDE Plasma 6 session.

## Install

    make build
    sudo make install              # binary, systemd unit, desktop entry
    sudo systemctl enable --now spoor
    make install-krunner           # optional, per user, needs no root

or build a Debian package with `make deb` and install that. By default the
daemon indexes `/home`; see [Choosing folders](#choosing-folders).
`sudo make uninstall` removes it again.

## Searching

    spoor query invoice                  # names containing "invoice"
    spoor query wedding jpg              # both words, anywhere in the path
    spoor query '"annual report"'        # a phrase, space included
    spoor query --path projects/2024     # match against the full path
    spoor query --regex '^IMG_[0-9]+\.jpg$'
    spoor query --case README --files
    spoor query --null invoice | xargs -0 ls -l

* **One word** matches file names. **Several words** are ANDed, and each may
  match anywhere in the full path -- the name or any folder above it.
  **Quotes** keep a phrase together.
* A `/` in the query matches against the path; a trailing `/` (`photos/`)
  means "inside that folder".
* Case is ignored unless Match Case is on, for every script: `отчёт` finds
  `Отчёт.pdf`.
* **Regex** mode treats the whole query as one expression (the `regex` crate:
  linear time, size-capped, since any local user can send patterns).
* `spoor query` prints paths as raw bytes, one per line; `--null` ends each
  with a NUL instead, for names that contain a newline.

| option | GUI | CLI |
|---|---|---|
| Match Case | Ctrl+I | `--case` |
| Enable Regex | Ctrl+R | `--regex` |
| Search in Path | Ctrl+U | `--path` |
| Files only / Folders only | Search menu | `--files` / `--folders` |
| Hide dotfiles | Preferences | `--no-hidden` |

The GUI (`spoor gui`, or "File Search" in the application menu) keeps these in
`~/.config/spoor/gui.conf`. Double-click or Enter opens a result; click a
column header to sort; right-click offers Open With, Open Containing Folder,
Copy Path, Copy Name, Move to Trash and Properties (the file manager's own
dialog, via `org.freedesktop.FileManager1`). With the KRunner plugin,
Alt+Space finds files from anywhere.

## Choosing folders

**Edit → Preferences** in the window lists the folders to index, folders to
leave out, and network folders to walk on a timer. **Apply Folder Changes…**
asks for an administrator password (polkit), saves `/etc/spoor/spoor.conf` and
restarts the index. The file can also be written by hand:

    root = /home
    root = /srv/data
    exclude = /home/you/.cache
    rescan = /home/you/GoogleDrive
    rescan_interval = 900

`sudo spoor configure FILE` checks such a file, installs it and restarts the
service. Each folder gets its own fanotify mark, so folders on different disks
are all watched live. A folder inside another folder on the same filesystem is
merged into it. A folder on a disk that is not attached is skipped until the
next restart.

## Network and FUSE mounts

fanotify only sees changes that pass through the local kernel, and the walk
stays on one filesystem, so a mount such as an rclone Google Drive is not
indexed by default. Listed under **Network folders** (`rescan =`), it is
walked every `rescan_interval` seconds (default 900) instead; it must lie inside
an indexed folder. A path that is not currently mounted is never walked -- it
would read as an empty folder -- and is retried every minute.

A FUSE mount is private to its owner, so root cannot read it by default.
rclone accepts `--allow-root` but silently ignores it (its FUSE library dropped
support). What works is `--allow-other --default-permissions --umask 077`: the
kernel then enforces owner-only file modes, so other accounts -- service
accounts included -- are refused while root reads through its capabilities.
Plain `--allow-other` would expose the mount to every local account. Both need
`user_allow_other` in `/etc/fuse.conf`; confirm with `findmnt` that the kernel
options show `allow_other,default_permissions`.

The first walk after rclone starts goes to the provider's API and may be
throttled heavily (Google Drive with rclone's shared client ID: 8-25 entries/s;
create your own client ID). Results are merged only when a walk completes.

## Security

The daemon runs as root and can list every name under the indexed tree; any
local account may query it. Each result is shown only if the caller could list
its folder: the answering thread takes on the caller's uid, gid and groups
(read from the socket, not from the request) and asks the kernel. The
snapshot under `/var/lib/spoor` is readable by root only; changing which
folders are indexed needs an administrator password; the socket's work
per client is bounded, and kernel records are parsed with explicit bounds
checks. See [SECURITY.md](SECURITY.md) for the details and for reporting
vulnerabilities.

## Socket protocol

On `/run/spoor.sock`, one request line per connection:

    SEARCH0 <limit> <flags> <pattern>  -> each path as raw bytes ending in NUL;
                                          an empty record ends the reply
    SEARCH <limit> <flags> <pattern>   -> one path per line, blank line ends
    QUERY <limit> <pattern>            -> SEARCH with no flags
    STATS                              -> "entries <n> arena <n>"

Flags are any of `c` (match case), `r` (regex), `p` (search in path), `f`
(files only), `d` (folders only), `h` (hide dotfiles), or `-` for none. Errors
come back as a record `ERR <message>`; paths always start with `/`. The line
forms withhold paths containing a newline; use `SEARCH0`.

## Known gaps

* On btrfs, a subvolume's directory entry reports the same inode number as the
  mount root. Letting it into the inode map put files in the wrong folder; the
  fix (foreign directories take no inode) is unit-tested, and a live re-test on
  btrfs is still outstanding.
* Deleted entries keep their memory until the daily reconciliation swaps in a
  freshly built index (or the daemon restarts).
* A multi-word query whose words are all shorter than 3 bytes falls back to a
  full scan (up to ~0.9 s on 2.3M files when matches are rare).
* Case folding is per character (`ß` does not match `ss`), and in regex mode
  `.` matches a character, not a byte that is not part of valid UTF-8.
* Copy Path and Copy Name put text on the clipboard, so a name that is not
  UTF-8 is copied with its stray bytes replaced.

## License

Licensed under either of the Apache License, Version 2.0
([LICENSE-APACHE](LICENSE-APACHE)) or the MIT license
([LICENSE-MIT](LICENSE-MIT)), at your option.
