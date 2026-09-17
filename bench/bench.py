#!/usr/bin/env python3
"""Benchmark spoor against plocate, FSearch, find, fd, bfs and eBPF.

Everything runs on one synthetic tree, so anyone can reproduce the numbers:

  sudo python3 bench/bench.py            # writes bench/results.json
  uv run --with matplotlib bench/plot.py # draws docs/benchmark-*.png

The tree lives on its own ext4 image, loop-mounted, because a filesystem the
installed spoor service already watches would carry fanotify's cost into every
measurement, including the ones that are supposed to have no watcher at all.

Root is needed for spoor's daemon (fanotify), for the eBPF watcher, for
`updatedb`, and to mount the image. The searches run as the user who invoked
sudo or pkexec, as they would in everyday use, through the command line each
tool ships. FSearch has no command line, so it runs as that user on a hidden X
display with its own D-Bus session, driven by `fsearch -s`.

Measured:
  search    wall time of one search, process start to last result, median
  fresh     time from creating a file until each tool can report it
  burst     files created in quick nested bursts: how many each watcher saw
  build     time to index the tree from nothing; memory and disk used
  watch     time and marks needed before changes are noticed
  overhead  what watching costs everything else: 120,000 file operations
"""

import argparse
import json
import os
import platform
import pwd
import random
import re
import select
import shutil
import signal
import socket
import statistics
import subprocess
import sys
import threading
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
# the file name, and their hit counts are checked against each other.
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


def own(path, user):
    for root, dirs, files in os.walk(path):
        for name in dirs + files:
            os.chown(os.path.join(root, name), user.pw_uid, user.pw_gid, follow_symlinks=False)
    os.chown(path, user.pw_uid, user.pw_gid)


def rss_mb(pid):
    try:
        for line in open(f"/proc/{pid}/status"):
            if line.startswith("VmRSS:"):
                return int(line.split()[1]) / 1024
    except OSError:
        return None


# ---------------------------------------------------------------- corpus

def mount_image(image, mnt, size_gb, inodes):
    """A filesystem of our own: nothing else watches it, so "no watcher at
    all" is a real measurement rather than one taken under spoor's mark."""
    os.makedirs(mnt, exist_ok=True)
    if os.path.ismount(mnt):
        return
    if not os.path.exists(image):
        log(f"creating {size_gb} GB ext4 image at {image}")
        with open(image, "wb") as f:
            f.truncate(size_gb * 2**30)
        subprocess.run(["mkfs.ext4", "-q", "-F", "-N", str(inodes),
                        "-E", "lazy_itable_init=0,lazy_journal_init=0", image], check=True)
    subprocess.run(["mount", "-o", "loop,noatime", image, mnt], check=True)
    os.chmod(mnt, 0o755)


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
        took = None
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
        threading.Thread(target=self.proc.stderr.read, daemon=True).start()
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

    def stop(self):
        if self.proc:
            self.proc.send_signal(signal.SIGTERM)
            self.proc.wait()
            self.proc = None


# ---------------------------------------------------------------- searches

def search_commands(spoor_bin, sock, db, root, fd_bin, pattern):
    cmds = {
        "spoor": [spoor_bin, "query", "--socket", sock, "--limit", str(CAP), pattern],
        "plocate": ["plocate", "-d", db, "-i", "-b", "-l", str(CAP), pattern],
        # Neither find nor bfs can stop at a count; head closes the pipe.
        "find": ["sh", "-c", f'find "$1" -iname "*$2*" | head -n {CAP}', "sh", root, pattern],
        "bfs": ["sh", "-c", f'bfs "$1" -iname "*$2*" | head -n {CAP}', "sh", root, pattern],
    }
    if fd_bin:
        cmds["fd"] = [fd_bin, "--unrestricted", "--ignore-case", "--fixed-strings",
                      "--max-results", str(CAP), pattern, root]
    return cmds


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


# ---------------------------------------------------------------- FSearch

FSEARCH_CONF = """[Database]
update_database_on_launch=true
update_database_every=false
exclude_hidden_files_and_folders=false
location_1={root}
location_enabled_1=true
location_update_1=true
location_one_filesystem_1=false

[Search]
search_as_you_type=true
auto_match_case=false
auto_search_in_path=false
match_case=false
enable_regex=false
search_in_path=false

[Interface]
show_indexing_status=false
"""


def run_fsearch(spec, work, user):
    """FSearch only searches from its window, so drive that window: a hidden X
    display, its own D-Bus session, and `fsearch -s` to set each search."""
    base = os.path.join(work, "fsearch")
    os.makedirs(base, exist_ok=True)
    spec_path = os.path.join(base, "spec.json")
    out_path = os.path.join(base, "out.json")
    spec = dict(spec, base=base, out=out_path)
    json.dump(spec, open(spec_path, "w"))
    if os.path.exists(out_path):
        os.unlink(out_path)
    own(base, user)
    env = {"HOME": user.pw_dir, "USER": user.pw_name, "LOGNAME": user.pw_name,
           "PATH": "/usr/local/bin:/usr/bin:/bin", "LANG": "C.UTF-8"}
    r = subprocess.run(["dbus-run-session", "--", "xvfb-run", "-a", sys.executable,
                        os.path.abspath(__file__), "--fsearch-client", spec_path],
                       env=env, cwd=base, preexec_fn=as_user(user),
                       stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if r.returncode != 0 or not os.path.exists(out_path):
        log(f"FSearch failed: {(r.stderr or '').strip()[-500:]}")
        return None
    return json.load(open(out_path))


def fsearch_client(spec_path):
    spec = json.load(open(spec_path))
    base = spec["base"]
    env = dict(os.environ,
               XDG_CONFIG_HOME=os.path.join(base, "cfg"),
               XDG_DATA_HOME=os.path.join(base, "data"),
               XDG_CACHE_HOME=os.path.join(base, "cache"),
               GDK_BACKEND="x11", GTK_USE_PORTAL="0", RUST_LOG="off",
               G_MESSAGES_DEBUG="fsearch-application fsearch-database fsearch-database-view")
    conf_dir = os.path.join(base, "cfg", "fsearch")
    os.makedirs(conf_dir, exist_ok=True)
    open(os.path.join(conf_dir, "fsearch.conf"), "w").write(
        FSEARCH_CONF.format(root=spec["corpus"]))
    db = os.path.join(base, "data", "fsearch", "fsearch.db")
    log_path = os.path.join(base, "fsearch.log")

    def wait_for(pattern, since=0, timeout=900):
        """Wait for a log line, returning its match and where the log now ends."""
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            text = open(log_path, errors="replace").read()
            m = re.search(pattern, text[since:])
            if m:
                return m, since + m.end()
            time.sleep(0.05)
        raise SystemExit(f"FSearch: never logged {pattern!r}")

    out = {"build_s": []}
    proc = None
    for _ in range(spec["builds"]):
        if proc:
            proc.terminate()
            proc.wait()
        for f in (db, log_path):
            if os.path.exists(f):
                os.unlink(f)
        open(log_path, "w").close()
        proc = subprocess.Popen(["fsearch"], env=env,
                                stdout=open(log_path, "a"), stderr=subprocess.STDOUT)
        # The first "update finished" is loading the (missing) database file;
        # the one after the save is the scan.
        _, pos = wait_for(r"\[db_save\] database file saved")
        m, pos = wait_for(r"\[app\] database update finished in ([\d.]+) ms", pos)
        out["build_s"].append(float(m.group(1)) / 1000)
    scanned, pos = wait_for(r"\[db_scan\] scanned: \d+ files, \d+ folders -> (\d+) total")
    out["entries"] = int(scanned.group(1))
    out["rss_mb"] = rss_mb(proc.pid)
    out["db_mb"] = os.path.getsize(db) / 2**20

    # Searches: each `fsearch -s` hands the pattern to the running window,
    # which logs how long the search took inside the app.
    done = r"\[query:[\d.]+\] finished in ([\d.]+) ms"
    times = {p: [] for p in spec["patterns"]}
    pos = len(open(log_path, errors="replace").read())
    for run in range(spec["runs"] + 2):
        for pat in spec["patterns"]:
            # `fsearch -s` hands the pattern to the running window, but the
            # client process then returns 1 and sometimes never exits. The
            # search still happens, so wait for the log, not for the process.
            client = subprocess.Popen(["fsearch", "-s", pat], env=env,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            m, pos = wait_for(done, pos, timeout=120)
            client.terminate()
            time.sleep(0.2)  # a follow-up search, if any, is the real one
            text = open(log_path, errors="replace").read()
            later = re.findall(done, text[pos:])
            if later:
                pos = len(text)
            if run >= 2:
                times[pat].append(float(later[-1] if later else m.group(1)) / 1000)
    out["search_s"] = {p: statistics.median(v) for p, v in times.items()}
    proc.terminate()
    proc.wait()
    json.dump(out, open(spec["out"], "w"))


# ---------------------------------------------------------------- watchers

class Watcher:
    """A watcher process speaking the watchers.c protocol."""

    def __init__(self, name, cmd, quiet=False):
        self.name = name
        self.proc = subprocess.Popen(
            cmd, stdout=subprocess.DEVNULL if quiet else subprocess.PIPE,
            stderr=subprocess.PIPE, bufsize=0)
        self.seen, self.buf = set(), b""
        if quiet:
            # READY went to the null sink; give it time to attach instead.
            self.marks, self.setup = 0, 0.0
            time.sleep(3)
            return
        line = self.proc.stdout.readline().split()
        if not line or line[0] != b"READY":
            err = self.proc.stderr.read(2000).decode(errors="replace")
            sys.exit(f"{name} watcher failed to start: {err}")
        self.marks, self.setup = int(line[1]), float(line[2])

    def pump(self, timeout):
        """Read the lines available now, waiting up to timeout for the first."""
        if select.select([self.proc.stdout], [], [], timeout)[0]:
            self.buf += os.read(self.proc.stdout.fileno(), 1 << 20)
            *lines, self.buf = self.buf.split(b"\n")
            self.seen.update(l for l in lines if not l.startswith(b"D "))

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
        """Stop it, and report how many events it had to drop, if it says."""
        self.proc.terminate()
        try:
            err = self.proc.communicate(timeout=10)[1] or b""
        except subprocess.TimeoutExpired:
            self.proc.kill()
            err = b""
        drops = re.search(rb"(\d+) events dropped", err)
        return int(drops.group(1)) if drops else 0


def spoor_wait_for(spoor, name, timeout=5.0):
    t_end = time.perf_counter() + timeout
    while time.perf_counter() < t_end:
        if spoor.request(f"QUERY 10 {name}"):
            return time.perf_counter()
        time.sleep(0.0002)
    return None


def freshness_watcher(w, root, trials):
    """A new file in a brand-new nested folder: the hard case for a watcher."""
    base = os.path.join(root, f"fresh-{w.name}")
    os.makedirs(base, exist_ok=True)
    w.drain(0.3)
    got = []
    for i in range(trials):
        d = os.path.join(base, f"t{i}", "a", "b")
        path = os.path.join(d, f"f{i}.txt")
        t0 = time.perf_counter()
        os.makedirs(d)
        os.close(os.open(path, os.O_CREAT | os.O_WRONLY, 0o644))
        t = w.wait_for(path)
        got.append(None if t is None else t - t0)
    return got


def freshness_spoor(spoor, root, trials):
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


def burst(spoor, watchers, root, n):
    """n nested folders, each with a file, made as fast as Python can."""
    base = os.path.join(root, "burst")
    names = []
    for i in range(n):
        d = os.path.join(base, f"b{i}", "x", "y")
        os.makedirs(d)
        name = f"burst-{i}-wqzk.bin"
        os.close(os.open(os.path.join(d, name), os.O_CREAT | os.O_WRONLY, 0o644))
        names.append(os.path.join(d, name))
    time.sleep(3)
    out = {"created": n, "spoor": len(spoor.request(f"QUERY {CAP} wqzk.bin"))}
    for w in watchers:
        w.drain(1)
        out[w.name] = sum(1 for p in names if p.encode() in w.seen)
    return out


# ---------------------------------------------------------------- overhead

def churn(watchers_bin, root, ndirs, reps):
    """20 files created, renamed and deleted in each of ndirs new folders."""
    base = os.path.join(root, "churn")
    times = []
    for i in range(reps + 1):
        shutil.rmtree(base, ignore_errors=True)
        os.makedirs(base)
        out = subprocess.run([watchers_bin, "churn", base, str(ndirs)],
                             stdout=subprocess.PIPE, check=True, text=True).stdout
        if i > 0:  # the first pass warms the filesystem's own caches
            times.append(float(out))
    shutil.rmtree(base, ignore_errors=True)
    return statistics.median(times)


# ---------------------------------------------------------------- main

def build_tools(work):
    """The watchers, including the eBPF one (compiled here, not at load time)."""
    watchers = os.path.join(work, "watchers")
    subprocess.run(["cc", "-O2", "-o", watchers, os.path.join(HERE, "watchers.c")], check=True)
    ebpf = os.path.join(work, "ebpf_watch")
    if not all(shutil.which(t) for t in ("clang", "bpftool")):
        log("clang or bpftool missing: skipping the eBPF watcher")
        return watchers, None
    vmlinux = os.path.join(work, "vmlinux.h")
    if not os.path.exists(vmlinux):
        with open(vmlinux, "w") as f:
            subprocess.run(["bpftool", "btf", "dump", "file", "/sys/kernel/btf/vmlinux",
                            "format", "c"], stdout=f, check=True)
    obj = os.path.join(work, "ebpf_watch.bpf.o")
    subprocess.run(["clang", "-O2", "-g", "-Wall", "-Wno-missing-declarations",
                    "-target", "bpf", "-I", work, "-c",
                    os.path.join(HERE, "ebpf_watch.bpf.c"), "-o", obj], check=True)
    with open(os.path.join(work, "ebpf_watch.skel.h"), "w") as f:
        subprocess.run(["bpftool", "gen", "skeleton", obj], stdout=f, check=True)
    subprocess.run(["cc", "-O2", "-Wall", "-I", work, os.path.join(HERE, "ebpf_watch.c"),
                    "-lbpf", "-o", ebpf], check=True)
    return watchers, ebpf


def clean_extras(corpus):
    for extra in os.listdir(corpus):
        if extra.startswith(("fresh-", "burst", "churn")):
            shutil.rmtree(os.path.join(corpus, extra), ignore_errors=True)


def main():
    if len(sys.argv) == 5 and sys.argv[1] == "--socket-client":
        return socket_client(sys.argv[2], sys.argv[3], int(sys.argv[4]))
    if len(sys.argv) == 3 and sys.argv[1] == "--fsearch-client":
        return fsearch_client(sys.argv[2])

    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--dir", default="/var/tmp/spoor-bench")
    ap.add_argument("--files", type=int, default=1_000_000)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--runs", type=int, default=15, help="timed runs per search")
    ap.add_argument("--trials", type=int, default=50, help="freshness trials")
    ap.add_argument("--builds", type=int, default=3, help="index builds per tool")
    ap.add_argument("--churn-dirs", type=int, default=2000)
    ap.add_argument("--churn-reps", type=int, default=7)
    ap.add_argument("--burst", type=int, default=2000)
    ap.add_argument("--image-gb", type=int, default=8)
    ap.add_argument("--no-fsearch", action="store_true")
    ap.add_argument("--spoor", default=os.path.join(REPO, "target/release/spoor"))
    ap.add_argument("--out", default=os.path.join(HERE, "results.json"))
    a = ap.parse_args()

    if os.geteuid() != 0:
        sys.exit("needs root: sudo python3 bench/bench.py")
    user = invoking_user()
    for tool in ("plocate", "updatedb", "find", "bfs", "cc", "mkfs.ext4", "mount"):
        if not shutil.which(tool):
            sys.exit(f"{tool} not found")
    fd_bin = shutil.which("fdfind") or shutil.which("fd")
    if not fd_bin:
        log("fd not found: skipping it")
    if not os.access(a.spoor, os.X_OK):
        sys.exit(f"{a.spoor} missing: run 'make build' as your user first")

    mnt = os.path.join(a.dir, "mnt")
    corpus = os.path.join(mnt, "tree")
    work = os.path.join(a.dir, "work")
    os.makedirs(work, exist_ok=True)
    os.chmod(a.dir, 0o755)
    os.chmod(work, 0o755)
    mount_image(os.path.join(a.dir, "fs.img"), mnt, a.image_gb, max(a.files * 2, 200_000))
    make_corpus(corpus, a.files, a.seed)
    clean_extras(corpus)  # leftovers from an earlier run would skew the counts

    watchers_bin, ebpf_bin = build_tools(work)
    spoor = Spoor(a.spoor, corpus, work)
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
            "fsearch": subprocess.run(["fsearch", "--version"], capture_output=True,
                                      text=True).stdout.strip() or None,
        },
        "corpus": {"files": a.files, "seed": a.seed},
    }

    try:
        # --- build: plocate's database, then spoor's index
        db = os.path.join(work, "plocate.db")
        ub = []
        for _ in range(a.builds):
            if os.path.exists(db):
                os.unlink(db)  # updatedb reuses an old database; start from nothing
            t0 = time.monotonic()
            subprocess.run(["updatedb", "-U", corpus, "-o", db, "-l", "no",
                            "--prunepaths", "", "--prunenames", "", "--prunefs", ""],
                           check=True)
            ub.append(time.monotonic() - t0)
        os.chmod(db, 0o644)
        log(f"updatedb: {statistics.median(ub):.2f}s")

        sb = []
        for _ in range(a.builds):
            spoor.stop()
            took, entries = spoor.start()
            sb.append(took)
        spoor_rss = rss_mb(spoor.proc.pid)
        spoor.stop()
        log(f"spoor index: {statistics.median(sb):.2f}s, {entries:,} entries")

        # --- overhead: what each watcher costs everything else. spoor is
        # stopped for the others, or its mark would be in their numbers too.
        r["overhead"] = {"dirs": a.churn_dirs, "operations": a.churn_dirs * 61}
        r["overhead"]["nothing_s"] = churn(watchers_bin, corpus, a.churn_dirs, a.churn_reps)
        for name, cmd in (("inotify", [watchers_bin, "inotify", corpus]),
                          ("fanotify", [watchers_bin, "fanotify", corpus]),
                          ("ebpf", [ebpf_bin, corpus] if ebpf_bin else None)):
            if cmd is None:
                continue
            w = Watcher(name, cmd, quiet=True)
            r["overhead"][name + "_s"] = churn(watchers_bin, corpus, a.churn_dirs, a.churn_reps)
            r["overhead"][name + "_dropped"] = w.stop()
        spoor.start()
        r["overhead"]["spoor_s"] = churn(watchers_bin, corpus, a.churn_dirs, a.churn_reps)
        base = r["overhead"]["nothing_s"]
        log("overhead: " + ", ".join(
            f"{k[:-2]} +{(v / base - 1) * 100:.0f}%" for k, v in r["overhead"].items()
            if k.endswith("_s") and k != "nothing_s"))

        # --- search
        r["search"] = []
        for label, pat in QUERIES:
            row = {"label": label, "pattern": pat}
            for tool, cmd in search_commands(a.spoor, spoor.sock, db, corpus, fd_bin, pat).items():
                row[tool + "_s"], row[tool + "_hits"] = time_search(cmd, user, a.runs)
            row["spoor_socket_s"], row["spoor_socket_hits"] = time_socket(
                spoor.sock, pat, user, a.runs)
            counts = {k[:-5]: v for k, v in row.items() if k.endswith("_hits")}
            log(f"search {label!r}: " + ", ".join(
                f"{k[:-2]} {v * 1000:.1f}ms" for k, v in row.items() if k.endswith("_s")))
            if len(set(counts.values())) != 1:
                log(f"  WARNING: tools disagree on hits for {pat!r}: {counts}")
            r["search"].append(row)

        # --- FSearch: its own index, its own window
        if not a.no_fsearch and shutil.which("fsearch") and shutil.which("xvfb-run"):
            log("FSearch: building its database and timing searches")
            r["fsearch"] = run_fsearch(
                {"corpus": corpus, "patterns": [p for _, p in QUERIES],
                 "runs": a.runs, "builds": a.builds}, work, user)
            if r["fsearch"]:
                log(f"FSearch: build {statistics.median(r['fsearch']['build_s']):.2f}s, "
                    "searches " + ", ".join(
                        f"{v * 1000:.1f}ms" for v in r["fsearch"]["search_s"].values()))

        # --- watch setup and freshness, each watcher on its own
        r["watch"] = {"max_user_watches": int(
            open("/proc/sys/fs/inotify/max_user_watches").read())}
        r["fresh"] = {"spoor_s": freshness_spoor(spoor, corpus, a.trials)}
        live = []
        for name, cmd in (("inotify", [watchers_bin, "inotify", corpus]),
                          ("fanotify", [watchers_bin, "fanotify", corpus]),
                          ("ebpf", [ebpf_bin, corpus] if ebpf_bin else None)):
            if cmd is None:
                continue
            w = Watcher(name, cmd)
            r["watch"][name + "_marks"] = w.marks
            r["watch"][name + "_setup_s"] = w.setup
            if name != "fanotify":  # spoor's own number stands in for fanotify
                r["fresh"][name + "_s"] = freshness_watcher(w, corpus, a.trials)
            live.append(w)
        log("watch: " + ", ".join(f"{k} {v}" for k, v in r["watch"].items()))
        for k, v in r["fresh"].items():
            good = [x for x in v if x is not None]
            log(f"fresh {k[:-2]}: median {statistics.median(good) * 1000:.2f}ms, "
                f"{len(v) - len(good)} never seen")
        r["fresh"]["plocate"] = "next updatedb (daily)"
        r["fresh"]["fsearch"] = "next rescan"
        r["fresh"]["find_s"] = r["search"][0]["find_s"]

        r["burst"] = burst(spoor, live, corpus, a.burst)
        log(f"burst: {r['burst']}")
        for w in live:
            dropped = w.stop()
            if dropped:
                r["burst"][w.name + "_dropped"] = dropped

        r["build"] = {
            "entries": entries,
            "spoor_s": statistics.median(sb),
            "updatedb_s": statistics.median(ub),
            "spoor_rss_mb": spoor_rss,
            "plocate_db_mb": os.path.getsize(db) / 2**20,
        }
        if r.get("fsearch"):
            r["build"]["fsearch_s"] = statistics.median(r["fsearch"]["build_s"])
            r["build"]["fsearch_rss_mb"] = r["fsearch"]["rss_mb"]
            r["build"]["fsearch_db_mb"] = r["fsearch"]["db_mb"]
    finally:
        spoor.stop()
        clean_extras(corpus)

    with open(a.out, "w") as f:
        json.dump(r, f, indent=2)
    os.chown(a.out, user.pw_uid, user.pw_gid)
    log(f"wrote {a.out}")


if __name__ == "__main__":
    main()
