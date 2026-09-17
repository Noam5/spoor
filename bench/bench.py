#!/usr/bin/env python3
"""Benchmark spoor against plocate, find and a plain inotify watcher.

Everything runs on one synthetic tree, so anyone can reproduce the numbers:

  sudo python3 bench/bench.py            # writes bench/results.json
  uv run --with matplotlib bench/plot.py # draws docs/benchmark.png

Root is needed for spoor's daemon (fanotify) and for plocate's updatedb. The
searches themselves run as the user who invoked sudo or pkexec, as they would
in everyday use, and through the same command line each tool ships.

Measured:
  search    wall time of one search, process start to last result, median
  fresh     time from creating a file until each tool can report it
  burst     files created in quick nested bursts: how many each tool saw
  build     time to index the tree from nothing; memory and disk used
  watch     time and watches needed before changes are noticed
"""

import argparse
import json
import os
import platform
import pwd
import random
import select
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import time

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(HERE)

# Word frequencies follow a rough Zipf curve, like real file names: a few words
# are everywhere, most are rare. Queries below are chosen across that range.
WORDS = """
report invoice photo draft final backup notes budget resume letter scan
contract summary slides meeting receipt statement project thesis chapter
lecture recording screenshot export archive manual guide recipe travel
wedding holiday family passport insurance mortgage payslip tax schedule
agenda minutes proposal design mockup logo banner poster flyer brochure
catalog inventory order shipping tracking warranty license certificate
diploma transcript portfolio sketch render model texture shader sound track
album playlist podcast episode season trailer subtitle font theme wallpaper
icon sprite level save config settings profile session cache index log
""".split()
EXTS = ["pdf", "jpg", "png", "txt", "md", "docx", "xlsx", "mp3", "mp4",
        "zip", "json", "rs", "py", "c", "h", "html", "css", "js", "svg", "log"]

# (label, pattern). Every tool is asked for a case-insensitive substring of
# the file name, and its hit count is checked against the others.
QUERIES = [
    ("one file by name", "zqxv-unique-needle.pdf"),
    ("a word", "passport"),
    ("a common word", "report"),
    ("an extension", ".xlsx"),
    ("two letters", "zq"),
]
CAP = 100_000  # spoor's largest result window; every tool gets the same cap


def log(msg):
    print(f"[bench] {msg}", file=sys.stderr, flush=True)


def invoking_user():
    uid = os.environ.get("PKEXEC_UID") or os.environ.get("SUDO_UID")
    if uid is None:
        sys.exit("run through sudo or pkexec, so searches can run as you")
    return pwd.getpwuid(int(uid))


def as_user(user):
    def demote():
        os.initgroups(user.pw_name, user.pw_gid)
        os.setgid(user.pw_gid)
        os.setuid(user.pw_uid)
    return demote


# ---------------------------------------------------------------- corpus

def make_corpus(root, nfiles, seed):
    stamp = os.path.join(root, ".corpus")
    want = f"{nfiles} {seed}\n"
    if os.path.exists(stamp) and open(stamp).read() == want:
        log(f"reusing corpus at {root}")
        return
    if os.path.exists(root):
        shutil.rmtree(root)
    log(f"creating {nfiles:,} files under {root}")
    rng = random.Random(seed)
    weights = [1 / (i + 1) for i in range(len(WORDS))]
    dirs, made, t0 = [root], 0, time.monotonic()
    os.makedirs(root)
    while made < nfiles:
        # Folders of 1-40 entries, nested up to 8 deep, as in a home folder.
        parent = rng.choice(dirs[-2000:]) if len(dirs) > 1 else root
        if parent.count("/") - root.count("/") >= 8:
            parent = root
        d = os.path.join(parent, "-".join(rng.choices(WORDS, weights, k=2)) + f"-{len(dirs)}")
        os.mkdir(d)
        dirs.append(d)
        for _ in range(rng.randint(1, 40)):
            name = "_".join(rng.choices(WORDS, weights, k=2))
            name += f"-{rng.randrange(10**6):06d}.{rng.choice(EXTS)}"
            os.close(os.open(os.path.join(d, name), os.O_CREAT | os.O_WRONLY, 0o644))
            made += 1
    os.close(os.open(os.path.join(dirs[len(dirs) // 2], "zqxv-unique-needle.pdf"),
                     os.O_CREAT | os.O_WRONLY, 0o644))
    open(stamp, "w").write(want)
    log(f"corpus: {made:,} files in {len(dirs):,} folders, {time.monotonic() - t0:.0f}s")


# ---------------------------------------------------------------- spoor

class Spoor:
    def __init__(self, spoor_bin, root, work):
        self.bin, self.root = spoor_bin, root
        self.sock = os.path.join(work, "spoor.sock")
        self.state = os.path.join(work, "index.bin")
        self.proc = None

    def start(self):
        """Start from nothing; return (seconds to index, entries)."""
        for p in (self.sock, self.state):
            if os.path.exists(p):
                os.unlink(p)
        t0 = time.monotonic()
        self.proc = subprocess.Popen(
            [self.bin, "daemon", "--root", self.root, "--socket", self.sock,
             "--state", self.state, "--save-interval", "0",
             "--reconcile-interval", "0"],
            stderr=subprocess.PIPE, text=True)
        for line in self.proc.stderr:
            if line.startswith("spoor: indexed"):
                took = time.monotonic() - t0
            if line.startswith("spoor: ready"):
                break
        else:
            sys.exit("spoor daemon exited before it was ready")
        # Keep draining stderr so the daemon never blocks on a full pipe.
        subprocess.Popen(["cat"], stdin=self.proc.stderr, stdout=subprocess.DEVNULL)
        entries = int(self.request("STATS")[0].split()[1])
        return took, entries

    def request(self, line):
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(self.sock)
            s.sendall(line.encode() + b"\n")
            buf = b""
            while not buf.endswith(b"\n\n") and buf != b"\n":
                chunk = s.recv(65536)
                if not chunk:
                    break
                buf += chunk
        return buf.decode(errors="replace").splitlines()[:-1] if buf != b"\n" else []

    def rss_mb(self):
        for line in open(f"/proc/{self.proc.pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024

    def stop(self):
        if self.proc:
            self.proc.send_signal(signal.SIGTERM)
            self.proc.wait()
            self.proc = None


# ---------------------------------------------------------------- searches

def search_commands(spoor_bin, sock, db, root, pattern):
    return {
        "spoor": [spoor_bin, "query", "--socket", sock, "--limit", str(CAP), pattern],
        "plocate": ["plocate", "-d", db, "-i", "-b", "-l", str(CAP), pattern],
        # find has no limit flag; head closes the pipe at the cap.
        "find": ["sh", "-c", f'find "$1" -iname "*$2*" | head -n {CAP}', "sh", root, pattern],
    }


def time_search(cmd, user, runs):
    times, hits = [], None
    for i in range(runs + 2):
        t0 = time.perf_counter()
        out = subprocess.run(cmd, stdout=subprocess.PIPE, check=True, cwd="/",
                             preexec_fn=as_user(user)).stdout
        dt = time.perf_counter() - t0
        n = out.count(b"\n")
        if i >= 2:  # two warm-up runs: the page cache is warm for every tool
            times.append(dt)
        if hits is not None and n != hits:
            sys.exit(f"{cmd[0]}: result count changed between runs")
        hits = n
    return statistics.median(times), hits


def time_socket(sock, pattern, user, runs):
    """spoor's search as the window and KRunner see it: one request on an open
    socket, no process to start. Runs as the user, so permission checks count."""
    out = subprocess.run(
        [sys.executable, os.path.abspath(__file__), "--socket-client", sock, pattern, str(runs)],
        stdout=subprocess.PIPE, check=True, cwd="/", preexec_fn=as_user(user), text=True).stdout
    median, hits = out.split()
    return float(median), int(hits)


def socket_client(sock, pattern, runs):
    req = f"SEARCH0 {CAP} - {pattern}\n".encode()
    times = []
    for i in range(runs + 2):
        t0 = time.perf_counter()
        with socket.socket(socket.AF_UNIX) as s:
            s.connect(sock)
            s.sendall(req)
            buf = bytearray()
            while not buf.endswith(b"\0\0") and buf != b"\0":
                chunk = s.recv(1 << 20)
                if not chunk:
                    break
                buf += chunk
        if i >= 2:
            times.append(time.perf_counter() - t0)
    print(statistics.median(times), buf.count(b"\0") - 1)


# ---------------------------------------------------------------- freshness

class Inotify:
    def __init__(self, watchers_bin, root):
        self.proc = subprocess.Popen([watchers_bin, "inotify", root],
                                     stdout=subprocess.PIPE, bufsize=0)
        line = self.proc.stdout.readline().split()
        if not line or line[0] != b"READY":
            sys.exit("inotify watcher failed to start")
        self.watches, self.setup = int(line[1]), float(line[2])
        self.seen, self.buf = set(), b""

    def pump(self, timeout):
        """Read the lines available now, waiting up to timeout for the first."""
        if select.select([self.proc.stdout], [], [], timeout)[0]:
            self.buf += os.read(self.proc.stdout.fileno(), 1 << 20)
            *lines, self.buf = self.buf.split(b"\n")
            self.seen.update(lines)

    def drain(self, quiet):
        """Read until nothing new arrives for `quiet` seconds."""
        while select.select([self.proc.stdout], [], [], quiet)[0]:
            self.pump(0)

    def wait_for(self, path, timeout=5.0):
        t_end = time.perf_counter() + timeout
        while path.encode() not in self.seen:
            left = t_end - time.perf_counter()
            if left <= 0:
                return None
            self.pump(left)
        return time.perf_counter()

    def stop(self):
        self.proc.kill()
        self.proc.wait()


def spoor_wait_for(spoor, name, timeout=5.0):
    t_end = time.perf_counter() + timeout
    while time.perf_counter() < t_end:
        if spoor.request(f"QUERY 10 {name}"):
            return time.perf_counter()
        time.sleep(0.0002)
    return None


def freshness_inotify_alone(ino, root, trials):
    """A new file in a brand-new nested folder: the hard case for a watcher."""
    base = os.path.join(root, "fresh-ino")
    os.makedirs(base, exist_ok=True)
    ino.drain(0.2)
    got = []
    for i in range(trials):
        d = os.path.join(base, f"t{i}", "a", "b")
        path = os.path.join(d, f"f{i}.txt")
        t0 = time.perf_counter()
        os.makedirs(d)
        os.close(os.open(path, os.O_CREAT | os.O_WRONLY, 0o644))
        t = ino.wait_for(path)
        got.append(None if t is None else t - t0)
    return got


def freshness_spoor_alone(spoor, root, trials):
    base = os.path.join(root, "fresh-spoor")
    os.makedirs(base, exist_ok=True)
    got = []
    for i in range(trials):
        name = f"g{os.getpid()}-{i}-kqzv.txt"
        d = os.path.join(base, f"t{i}", "a", "b")
        t0 = time.perf_counter()
        os.makedirs(d)
        os.close(os.open(os.path.join(d, name), os.O_CREAT | os.O_WRONLY, 0o644))
        t = spoor_wait_for(spoor, name)
        got.append(None if t is None else t - t0)
    return got


def burst(spoor, ino, root, n):
    """n nested folders, each with a file, made as fast as Python can."""
    base = os.path.join(root, "burst")
    names = []
    for i in range(n):
        d = os.path.join(base, f"b{i}", "x", "y")
        os.makedirs(d)
        name = f"burst-{i}-wqzk.bin"
        os.close(os.open(os.path.join(d, name), os.O_CREAT | os.O_WRONLY, 0o644))
        names.append(os.path.join(d, name))
    time.sleep(2)
    ino.drain(1)
    ino_seen = sum(1 for p in names if p.encode() in ino.seen)
    spoor_seen = len(spoor.request(f"QUERY {CAP} wqzk.bin"))
    return {"created": n, "spoor": spoor_seen, "inotify": ino_seen}


# ---------------------------------------------------------------- main

def main():
    if len(sys.argv) == 5 and sys.argv[1] == "--socket-client":
        return socket_client(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", default="/var/tmp/spoor-bench")
    ap.add_argument("--files", type=int, default=1_000_000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--runs", type=int, default=15, help="timed runs per search")
    ap.add_argument("--trials", type=int, default=50, help="freshness trials")
    ap.add_argument("--builds", type=int, default=3, help="index builds per tool")
    ap.add_argument("--spoor", default=os.path.join(REPO, "target/release/spoor"))
    ap.add_argument("--out", default=os.path.join(HERE, "results.json"))
    a = ap.parse_args()

    if os.geteuid() != 0:
        sys.exit("needs root: sudo python3 bench/bench.py")
    user = invoking_user()
    for tool in ("plocate", "updatedb", "find", "cc"):
        if not shutil.which(tool):
            sys.exit(f"{tool} not found")
    if not os.access(a.spoor, os.X_OK):
        sys.exit(f"{a.spoor} missing: run 'make build' as your user first")

    corpus = os.path.join(a.dir, "tree")
    work = os.path.join(a.dir, "work")
    os.makedirs(work, exist_ok=True)
    os.chmod(a.dir, 0o755)
    os.chmod(work, 0o755)
    make_corpus(corpus, a.files, a.seed)
    # Leftovers from an earlier run's change tests would skew the counts.
    for extra in ("fresh", "fresh-ino", "fresh-spoor", "burst"):
        shutil.rmtree(os.path.join(corpus, extra), ignore_errors=True)

    watchers = os.path.join(work, "watchers")
    subprocess.run(["cc", "-O2", "-o", watchers, os.path.join(HERE, "watchers.c")], check=True)

    r = {
        "machine": {
            "kernel": platform.release(),
            "cpu": next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo")
                         if l.startswith("model name")), "?"),
            "cores": os.cpu_count(),
            "filesystem": subprocess.run(["findmnt", "-no", "FSTYPE", "-T", corpus],
                                         capture_output=True, text=True).stdout.strip(),
            "plocate": subprocess.run(["plocate", "--version"], capture_output=True,
                                      text=True).stdout.splitlines()[0],
            "spoor": subprocess.run([a.spoor, "--version"], capture_output=True,
                                    text=True).stdout.strip(),
        },
        "corpus": {"files": a.files, "seed": a.seed},
    }

    # --- build
    db = os.path.join(work, "plocate.db")
    ub = []
    for i in range(a.builds):
        if os.path.exists(db):
            os.unlink(db)  # updatedb reuses an old database; start from nothing
        t0 = time.monotonic()
        subprocess.run(["updatedb", "-U", corpus, "-o", db, "-l", "no",
                        "--prunepaths", "", "--prunenames", "", "--prunefs", ""],
                       check=True)
        ub.append(time.monotonic() - t0)
    os.chmod(db, 0o644)
    log(f"updatedb: {statistics.median(ub):.2f}s")

    spoor = Spoor(a.spoor, corpus, work)
    sb = []
    for i in range(a.builds):
        spoor.stop()
        took, entries = spoor.start()
        sb.append(took)
    log(f"spoor index: {statistics.median(sb):.2f}s, {entries:,} entries")
    r["build"] = {
        "entries": entries,
        "spoor_s": statistics.median(sb),
        "updatedb_s": statistics.median(ub),
        "spoor_rss_mb": spoor.rss_mb(),
        "plocate_db_mb": os.path.getsize(db) / 2**20,
    }

    # --- search
    r["search"] = []
    for label, pat in QUERIES:
        row = {"label": label, "pattern": pat}
        for tool, cmd in search_commands(a.spoor, spoor.sock, db, corpus, pat).items():
            row[tool + "_s"], row[tool + "_hits"] = time_search(cmd, user, a.runs)
        row["spoor_socket_s"], row["spoor_socket_hits"] = time_socket(spoor.sock, pat, user, a.runs)
        log(f"search {label!r}: " + ", ".join(
            f"{t} {row[t + '_s'] * 1000:.1f}ms/{row[t + '_hits']}"
            for t in ("spoor_socket", "spoor", "plocate", "find")))
        if len({row[t + "_hits"] for t in ("spoor_socket", "spoor", "plocate", "find")}) != 1:
            log(f"  WARNING: tools disagree on hits for {pat!r}")
        r["search"].append(row)

    # --- watch setup
    ino = Inotify(watchers, corpus)
    fan = subprocess.Popen([watchers, "fanotify", corpus], stdout=subprocess.PIPE)
    fan_line = fan.stdout.readline().split()
    fan.kill()
    fan.wait()
    r["watch"] = {
        "inotify_watches": ino.watches,
        "inotify_setup_s": ino.setup,
        "max_user_watches": int(open("/proc/sys/fs/inotify/max_user_watches").read()),
        "fanotify_marks": int(fan_line[1]),
        "fanotify_setup_s": float(fan_line[2]),
    }
    log(f"watch: inotify {ino.watches:,} watches in {ino.setup:.2f}s; "
        f"fanotify 1 mark in {float(fan_line[2]) * 1e6:.0f}us")

    # --- freshness, each watcher timed on its own
    r["fresh"] = {
        "spoor_s": freshness_spoor_alone(spoor, corpus, a.trials),
        "inotify_s": freshness_inotify_alone(ino, corpus, a.trials),
        # plocate knows only what the last updatedb saw; Debian and Ubuntu run
        # it once a day. find always sees the present, at the cost of a walk.
        "plocate": "next updatedb (daily)",
        "find_s": r["search"][0]["find_s"],
    }
    for k in ("spoor_s", "inotify_s"):
        v = [x for x in r["fresh"][k] if x is not None]
        log(f"fresh {k[:-2]}: median {statistics.median(v) * 1000:.2f}ms, "
            f"{len(r['fresh'][k]) - len(v)} never seen")

    r["burst"] = burst(spoor, ino, corpus, 2000)
    log(f"burst: {r['burst']}")

    ino.stop()
    spoor.stop()
    for extra in ("fresh", "fresh-ino", "fresh-spoor", "burst"):
        shutil.rmtree(os.path.join(corpus, extra), ignore_errors=True)

    with open(a.out, "w") as f:
        json.dump(r, f, indent=2)
    os.chown(a.out, user.pw_uid, user.pw_gid)
    log(f"wrote {a.out}")


if __name__ == "__main__":
    main()
