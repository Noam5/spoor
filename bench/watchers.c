/*
 * The ways a Linux indexer can learn about new files, measured on the same
 * tree, plus the workload used to measure what watching costs.
 *
 *   watchers inotify DIR    watch every directory under DIR, one inotify
 *                           watch each, the way an unprivileged indexer must
 *   watchers fanotify DIR   one FAN_MARK_FILESYSTEM mark for DIR's whole
 *                           filesystem, as spoor does (needs root)
 *   watchers churn DIR N    in N new folders under DIR, create 20 files each,
 *                           rename them, delete everything; print the seconds
 *
 * The watchers print "READY <marks> <seconds>" once watching, then "<path>"
 * for each file or folder added and "D <path>" for each one removed. The eBPF
 * watcher (ebpf_watch.c) speaks the same protocol.
 *
 * The inotify side is written the way a careful watcher has to be, not as a
 * straw man: a directory created after the initial walk is watched and then
 * listed at once, so files made inside it before the watch took hold are still
 * reported instead of lost.
 */
#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/fanotify.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static double now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* ------------------------------------------------------------- inotify */

/* Watch descriptor -> directory path. Descriptors are small and dense. */
static char **paths;
static size_t npaths;
static long nwatches;

static void remember(int wd, const char *path)
{
	if ((size_t)wd >= npaths) {
		size_t n = npaths ? npaths : 1024;
		while (n <= (size_t)wd)
			n *= 2;
		paths = realloc(paths, n * sizeof *paths);
		memset(paths + npaths, 0, (n - npaths) * sizeof *paths);
		npaths = n;
	}
	free(paths[wd]);
	paths[wd] = strdup(path);
}

#define IN_MASK (IN_CREATE | IN_MOVED_TO | IN_DELETE | IN_MOVED_FROM | IN_ONLYDIR)

/* Watch DIR and everything below it. With report set, also print each entry
 * found: those appeared before their directory was watched. */
static void watch_tree(int fd, const char *dir, int report)
{
	int wd = inotify_add_watch(fd, dir, IN_MASK);
	if (wd < 0) {
		if (errno == ENOSPC) {
			fprintf(stderr, "watchers: out of inotify watches at %ld "
					"(fs.inotify.max_user_watches)\n", nwatches);
			exit(3);
		}
		return;
	}
	nwatches++;
	remember(wd, dir);

	DIR *d = opendir(dir);
	if (!d)
		return;
	struct dirent *e;
	char sub[PATH_MAX];
	while ((e = readdir(d))) {
		if (!strcmp(e->d_name, ".") || !strcmp(e->d_name, ".."))
			continue;
		snprintf(sub, sizeof sub, "%s/%s", dir, e->d_name);
		if (report)
			printf("%s\n", sub);
		if (e->d_type == DT_DIR)
			watch_tree(fd, sub, report);
	}
	closedir(d);
}

static int run_inotify(const char *root)
{
	double t0 = now();
	int fd = inotify_init1(IN_CLOEXEC);
	if (fd < 0) {
		perror("inotify_init1");
		return 1;
	}
	watch_tree(fd, root, 0);
	printf("READY %ld %.6f\n", nwatches, now() - t0);
	fflush(stdout);

	char buf[65536];
	for (;;) {
		ssize_t n = read(fd, buf, sizeof buf);
		if (n <= 0)
			return n < 0;
		for (char *p = buf; p < buf + n;) {
			struct inotify_event *ev = (struct inotify_event *)p;
			p += sizeof *ev + ev->len;
			if (ev->mask & IN_Q_OVERFLOW) {
				printf("OVERFLOW\n");
				continue;
			}
			if (ev->wd < 0 || (size_t)ev->wd >= npaths || !paths[ev->wd])
				continue;
			if (ev->mask & IN_IGNORED) { /* the directory is gone */
				free(paths[ev->wd]);
				paths[ev->wd] = NULL;
				nwatches--;
				continue;
			}
			if (!ev->len)
				continue;
			char full[PATH_MAX];
			snprintf(full, sizeof full, "%s/%s", paths[ev->wd], ev->name);
			if (ev->mask & (IN_DELETE | IN_MOVED_FROM)) {
				printf("D %s\n", full);
			} else {
				printf("%s\n", full);
				if (ev->mask & IN_ISDIR)
					watch_tree(fd, full, 1);
			}
		}
		fflush(stdout);
	}
}

/* ------------------------------------------------------------ fanotify */

static int run_fanotify(const char *root)
{
	double t0 = now();
	int fd = fanotify_init(FAN_CLASS_NOTIF | FAN_CLOEXEC | FAN_UNLIMITED_QUEUE |
				       FAN_REPORT_DFID_NAME, O_RDONLY);
	if (fd < 0) {
		perror("fanotify_init (needs root)");
		return 1;
	}
	if (fanotify_mark(fd, FAN_MARK_ADD | FAN_MARK_FILESYSTEM,
			  FAN_CREATE | FAN_MOVED_TO | FAN_DELETE | FAN_MOVED_FROM | FAN_ONDIR,
			  AT_FDCWD, root) < 0) {
		perror("fanotify_mark");
		return 1;
	}
	int mount_fd = open(root, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
	printf("READY 1 %.6f\n", now() - t0);
	fflush(stdout);

	/* Each event names a directory by file handle, plus an entry name. The
	 * handle is turned back into a path as spoor does: open it, then read
	 * the path of the open descriptor. */
	char buf[65536];
	for (;;) {
		ssize_t n = read(fd, buf, sizeof buf);
		if (n <= 0)
			return n < 0;
		struct fanotify_event_metadata *md = (struct fanotify_event_metadata *)buf;
		for (; FAN_EVENT_OK(md, n); md = FAN_EVENT_NEXT(md, n)) {
			if (md->mask & FAN_Q_OVERFLOW) {
				printf("OVERFLOW\n");
				continue;
			}
			struct fanotify_event_info_fid *fid = (struct fanotify_event_info_fid *)(md + 1);
			if (fid->hdr.info_type != FAN_EVENT_INFO_TYPE_DFID_NAME)
				continue;
			struct file_handle *fh = (struct file_handle *)fid->handle;
			const char *name = (const char *)(fh->f_handle + fh->handle_bytes);
			int dfd = open_by_handle_at(mount_fd, fh, O_PATH | O_CLOEXEC);
			if (dfd < 0)
				continue; /* the directory is already gone */
			char link[64], dir[PATH_MAX];
			snprintf(link, sizeof link, "/proc/self/fd/%d", dfd);
			ssize_t len = readlink(link, dir, sizeof dir - 1);
			close(dfd);
			if (len < 0)
				continue;
			dir[len] = 0;
			printf("%s%s/%s\n", md->mask & (FAN_DELETE | FAN_MOVED_FROM) ? "D " : "",
			       dir, name);
		}
		fflush(stdout);
	}
}

/* --------------------------------------------------------------- churn */

static int run_churn(const char *root, long ndirs)
{
	char dir[PATH_MAX], from[PATH_MAX + 32], to[PATH_MAX + 32];
	double t0 = now();
	for (long i = 0; i < ndirs; i++) {
		snprintf(dir, sizeof dir, "%s/churn-%ld", root, i);
		if (mkdir(dir, 0755) < 0) {
			perror(dir);
			return 1;
		}
		for (int j = 0; j < 20; j++) {
			snprintf(from, sizeof from, "%s/new-%d.txt", dir, j);
			int fd = open(from, O_CREAT | O_WRONLY | O_CLOEXEC, 0644);
			if (fd < 0) {
				perror(from);
				return 1;
			}
			close(fd);
		}
		for (int j = 0; j < 20; j++) {
			snprintf(from, sizeof from, "%s/new-%d.txt", dir, j);
			snprintf(to, sizeof to, "%s/renamed-%d.txt", dir, j);
			rename(from, to);
		}
		for (int j = 0; j < 20; j++) {
			snprintf(to, sizeof to, "%s/renamed-%d.txt", dir, j);
			unlink(to);
		}
		rmdir(dir);
	}
	printf("%.6f\n", now() - t0);
	return 0;
}

int main(int argc, char **argv)
{
	if (argc == 4 && !strcmp(argv[1], "churn"))
		return run_churn(argv[2], atol(argv[3]));
	if (argc != 3) {
		fprintf(stderr, "usage: watchers inotify|fanotify DIR\n"
				"       watchers churn DIR NDIRS\n");
		return 2;
	}
	if (!strcmp(argv[1], "inotify"))
		return run_inotify(argv[2]);
	if (!strcmp(argv[1], "fanotify"))
		return run_fanotify(argv[2]);
	fprintf(stderr, "watchers: unknown mode %s\n", argv[1]);
	return 2;
}
