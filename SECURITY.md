# Security

spoor runs a root daemon that can list every file name under the indexed
tree and answers queries from any local account, so its security model
matters more than most file-search tools'.

## Reporting a vulnerability

Please report privately through GitHub's **Report a vulnerability** button
on this repository's Security tab rather than in a public issue. Include the
version (`spoor` prints it in the GUI's About box, or see the package
version), the kernel version and filesystem, and steps to reproduce.

You should get an acknowledgement within a week. Fixes are released as soon
as they are ready, with credit unless you ask otherwise.

## What the daemon promises

* **A caller sees only what it could list itself.** Every result is checked
  against the caller's own credentials, taken from the socket
  (`SO_PEERCRED`, `SO_PEERGROUPS`) and not from anything the client says: the
  answering thread switches its filesystem uid, gid and groups to the caller's
  and asks the kernel whether it may search and read the parent directory
  (`faccessat2` with `AT_EACCESS`). If the switch fails, the query fails; if
  switching back fails, the daemon aborts rather than keep serving with the
  wrong identity.
* **The snapshot is root-only** (`/var/lib/spoor`, mode 0700, file 0600).
* **Bounded work per client:** at most 32 connections, 64 KiB requests,
  read and write timeouts, and linear-time regular expressions with a
  compiled-size cap, so a pattern cannot pin the index lock.
* **Kernel records are parsed defensively:** explicit bounds checks and
  unaligned reads, with a fuzz test over the parser. A panic aborts the
  process and systemd restarts it, rather than leaving a poisoned lock
  behind a daemon that still looks healthy.
* **Replies cannot be forged by file names:** the line protocols withhold
  paths containing a newline, and `SEARCH0` frames with NUL, the one byte a
  path cannot contain.

## Known limits

* Results are filtered on the parent directory's permissions at query
  time. A name that was listable when indexed but whose directory has since
  been locked down is withheld immediately; the index itself still holds it
  until the next event or reconciliation.
* The existence check is on names only. spoor never reads file contents, and
  size and dates in the GUI come from the viewer's own `stat()`.
* A rescanned mount (`--rescan`, e.g. an rclone drive) is walked as root.
  Configure it with `--allow-other --default-permissions --umask 077` as the
  README describes, so the kernel enforces owner-only modes that the
  per-caller filter can then rely on.
