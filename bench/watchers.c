/*
 * The two ways a Linux indexer can learn about new files, measured on the
 * same tree.
 *
 *   watchers inotify DIR    watch every directory under DIR, one inotify
 *                           watch each, the way an unprivileged indexer must
 *   watchers fanotify DIR   one FAN_MARK_FILESYSTEM mark, as spoor does
 *                           (needs root)
 *
 * Prints "READY <marks> <seconds>" once watching, then one line per file
 * created or moved in: "<path>". The harness times how long a line takes to
 * appear after it creates a file.
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

#define IN_MASK (IN_CREATE | IN_MOVED_TO | IN_ONLYDIR)

/* Watch DIR and everything below it. With report set, also print each file
 * found: those are files that appeared before their directory was watched. */
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
		if (e->d_type == DT_DIR)
			watch_tree(fd, sub, report);
		else if (report)
			printf("%s\n", sub);
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
			if (ev->wd < 0 || (size_t)ev->wd >= npaths || !paths[ev->wd] || !ev->len)
				continue;
			char full[PATH_MAX];
			snprintf(full, sizeof full, "%s/%s", paths[ev->wd], ev->name);
			if (ev->mask & IN_ISDIR)
				watch_tree(fd, full, 1);
			else
				printf("%s\n", full);
		}
		fflush(stdout);
	}
}

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
			  FAN_CREATE | FAN_MOVED_TO | FAN_ONDIR, AT_FDCWD, root) < 0) {
		perror("fanotify_mark");
		return 1;
	}
	printf("READY 1 %.6f\n", now() - t0);
	fflush(stdout);

	/* Resolving names is spoor's job; this mode only measures setup. */
	char buf[65536];
	while (read(fd, buf, sizeof buf) > 0)
		;
	return 0;
}

int main(int argc, char **argv)
{
	if (argc != 3) {
		fprintf(stderr, "usage: watchers inotify|fanotify DIR\n");
		return 2;
	}
	if (!strcmp(argv[1], "inotify"))
		return run_inotify(argv[2]);
	if (!strcmp(argv[1], "fanotify"))
		return run_fanotify(argv[2]);
	fprintf(stderr, "watchers: unknown mode %s\n", argv[1]);
	return 2;
}
