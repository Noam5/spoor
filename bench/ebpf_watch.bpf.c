// SPDX-License-Identifier: GPL-2.0 OR MIT
/*
 * Kernel half of the eBPF watcher: hooks the security checks every create,
 * rename and delete passes through, and sends the path to user space.
 *
 * These hooks run before the change is made, and whether or not anything is
 * watching the filesystem. fsnotify() would be the obvious hook, but the
 * kernel skips it entirely on a filesystem nobody watches with inotify or
 * fanotify, so an eBPF watcher cannot rely on it.
 */
#include "vmlinux.h"
#include <bpf/bpf_core_read.h>
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>

#define MAX_DEPTH 48
#define NAME_MAX 255
#define PATH_BUF 4096

/* Only changes on this filesystem are reported (kernel dev_t encoding). */
const volatile __u32 target_dev = 0;

enum { EV_ADD = 1, EV_REMOVE = 2 };

struct event {
	u8 op;
	u8 truncated;
	u16 len;
	/* Names from the leaf up to the filesystem root, each NUL-terminated. */
	char names[PATH_BUF + NAME_MAX + 1];
};

struct {
	__uint(type, BPF_MAP_TYPE_RINGBUF);
	__uint(max_entries, 64 << 20);
} events SEC(".maps");

struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, struct event);
} scratch SEC(".maps");

/* Events the ring buffer had no room for: each one means a rescan. */
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 1);
	__type(key, u32);
	__type(value, u64);
} dropped SEC(".maps");

static int emit(struct dentry *d, u8 op)
{
	u32 zero = 0;

	if (BPF_CORE_READ(d, d_sb, s_dev) != target_dev)
		return 0;
	struct event *e = bpf_map_lookup_elem(&scratch, &zero);
	if (!e)
		return 0;
	e->op = op;
	e->truncated = 1;

	u32 off = 0;
	for (int i = 0; i < MAX_DEPTH; i++) {
		struct dentry *parent = BPF_CORE_READ(d, d_parent);
		if (parent == d) { /* the filesystem root */
			e->truncated = 0;
			break;
		}
		if (off > PATH_BUF)
			break;
		long n = bpf_probe_read_kernel_str(e->names + off, NAME_MAX + 1,
						   BPF_CORE_READ(d, d_name.name));
		if (n <= 0)
			break;
		off += n;
		d = parent;
	}
	e->len = off;

	if (bpf_ringbuf_output(&events, e, offsetof(struct event, names) + off, 0)) {
		u64 *count = bpf_map_lookup_elem(&dropped, &zero);
		if (count)
			(*count)++;
	}
	return 0;
}

SEC("fentry/security_inode_create")
int BPF_PROG(on_create, struct inode *dir, struct dentry *dentry)
{
	return emit(dentry, EV_ADD);
}

SEC("fentry/security_inode_mkdir")
int BPF_PROG(on_mkdir, struct inode *dir, struct dentry *dentry)
{
	return emit(dentry, EV_ADD);
}

SEC("fentry/security_inode_mknod")
int BPF_PROG(on_mknod, struct inode *dir, struct dentry *dentry)
{
	return emit(dentry, EV_ADD);
}

SEC("fentry/security_inode_symlink")
int BPF_PROG(on_symlink, struct inode *dir, struct dentry *dentry)
{
	return emit(dentry, EV_ADD);
}

SEC("fentry/security_inode_link")
int BPF_PROG(on_link, struct dentry *old_dentry, struct inode *dir, struct dentry *new_dentry)
{
	return emit(new_dentry, EV_ADD);
}

SEC("fentry/security_inode_rename")
int BPF_PROG(on_rename, struct inode *old_dir, struct dentry *old_dentry,
	     struct inode *new_dir, struct dentry *new_dentry)
{
	emit(old_dentry, EV_REMOVE);
	return emit(new_dentry, EV_ADD);
}

SEC("fentry/security_inode_unlink")
int BPF_PROG(on_unlink, struct inode *dir, struct dentry *dentry)
{
	return emit(dentry, EV_REMOVE);
}

SEC("fentry/security_inode_rmdir")
int BPF_PROG(on_rmdir, struct inode *dir, struct dentry *dentry)
{
	return emit(dentry, EV_REMOVE);
}

char LICENSE[] SEC("license") = "Dual MIT/GPL";
