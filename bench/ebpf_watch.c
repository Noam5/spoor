/*
 * User half of the eBPF watcher. Same output as `watchers`:
 *
 *   ebpf_watch DIR     prints "READY <programs> <seconds>" once attached, then
 *                      "<path>" for each file or folder added anywhere on
 *                      DIR's filesystem and "D <path>" for each one removed
 *
 * The seconds cover opening, verifying, loading and attaching the programs;
 * compiling them happens earlier, at build time. Needs root.
 */
#include <errno.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <time.h>
#include <unistd.h>

#include <linux/types.h>

#include <bpf/bpf.h>
#include <bpf/libbpf.h>

#include "ebpf_watch.skel.h"

#define NAME_MAX_ 255
#define PATH_BUF 4096

struct event {
	unsigned char op;
	unsigned char truncated;
	unsigned short len;
	char names[PATH_BUF + NAME_MAX_ + 1];
};

static char mount_point[PATH_MAX];
static volatile sig_atomic_t stop;

static double now(void)
{
	struct timespec ts;
	clock_gettime(CLOCK_MONOTONIC, &ts);
	return ts.tv_sec + ts.tv_nsec / 1e9;
}

/* Where DIR's filesystem is mounted: the last folder on the way up that is
 * still on the same device. The kernel reports paths from there. */
static void find_mount_point(const char *dir)
{
	char path[PATH_MAX];
	struct stat st, up;
	if (!realpath(dir, path) || stat(path, &st) < 0) {
		perror(dir);
		exit(1);
	}
	while (strcmp(path, "/")) {
		char parent[PATH_MAX];
		snprintf(parent, sizeof parent, "%s", path);
		char *slash = strrchr(parent, '/');
		if (slash == parent)
			slash[1] = 0;
		else
			*slash = 0;
		if (stat(parent, &up) < 0 || up.st_dev != st.st_dev)
			break;
		snprintf(path, sizeof path, "%s", parent);
	}
	snprintf(mount_point, sizeof mount_point, "%s", strcmp(path, "/") ? path : "");
}

static int on_event(void *ctx, void *data, size_t size)
{
	(void)ctx;
	const struct event *e = data;
	if (size < 4 || e->len == 0 || e->truncated)
		return 0;

	/* Names arrive leaf first; print them root first. */
	const char *parts[64];
	int n = 0;
	for (unsigned off = 0; off < e->len && n < 64; off += strlen(e->names + off) + 1)
		parts[n++] = e->names + off;

	fputs(e->op == 2 ? "D " : "", stdout);
	fputs(mount_point, stdout);
	while (n--) {
		putchar('/');
		fputs(parts[n], stdout);
	}
	putchar('\n');
	return 0;
}

static void on_signal(int sig)
{
	(void)sig;
	stop = 1;
}

int main(int argc, char **argv)
{
	if (argc != 2) {
		fprintf(stderr, "usage: ebpf_watch DIR\n");
		return 2;
	}
	find_mount_point(argv[1]);
	struct stat st;
	stat(argv[1], &st);

	double t0 = now();
	struct ebpf_watch_bpf *skel = ebpf_watch_bpf__open();
	if (!skel) {
		perror("ebpf_watch: open");
		return 1;
	}
	/* The kernel packs dev_t as major << 20 | minor. */
	skel->rodata->target_dev = (major(st.st_dev) << 20) | minor(st.st_dev);
	if (ebpf_watch_bpf__load(skel) || ebpf_watch_bpf__attach(skel)) {
		fprintf(stderr, "ebpf_watch: load or attach failed: %s (needs root)\n", strerror(errno));
		return 1;
	}
	struct ring_buffer *rb =
		ring_buffer__new(bpf_map__fd(skel->maps.events), on_event, NULL, NULL);
	if (!rb) {
		perror("ebpf_watch: ring buffer");
		return 1;
	}
	int programs = 0;
	struct bpf_program *prog;
	bpf_object__for_each_program(prog, skel->obj)
		programs++;
	printf("READY %d %.6f\n", programs, now() - t0);
	fflush(stdout);

	signal(SIGINT, on_signal);
	signal(SIGTERM, on_signal);
	while (!stop) {
		int err = ring_buffer__poll(rb, 100);
		if (err < 0 && err != -EINTR)
			break;
		fflush(stdout);
	}

	unsigned ncpu = libbpf_num_possible_cpus();
	unsigned long long values[ncpu], total = 0;
	unsigned key = 0;
	if (!bpf_map_lookup_elem(bpf_map__fd(skel->maps.dropped), &key, values))
		for (unsigned i = 0; i < ncpu; i++)
			total += values[i];
	fprintf(stderr, "ebpf_watch: %llu events dropped\n", total);

	ring_buffer__free(rb);
	ebpf_watch_bpf__destroy(skel);
	return 0;
}
