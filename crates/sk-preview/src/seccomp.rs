// SPDX-License-Identifier: Apache-2.0
//! Inherited restrictions for helpers; see ADR 0034.

fn instruction(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

pub(crate) fn install() -> bool {
    #[cfg(target_arch = "x86_64")]
    let arch = 0xc000_003e;
    #[cfg(target_arch = "aarch64")]
    let arch = 0xc000_00b7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let arch = 0;
    let mut filter = vec![
        instruction(0x20, 0, 0, 4), // seccomp_data.arch
        instruction(0x15, 1, 0, arch),
        instruction(0x06, 0, 0, 0x8000_0000), // KILL_PROCESS on another ABI
        instruction(0x20, 0, 0, 0),           // seccomp_data.nr
        instruction(0x35, 0, 1, 0x4000_0000), // includes x32's alternate syscall bit
        instruction(0x06, 0, 0, 0x0005_0000 | libc::ENOSYS as u32),
    ];
    for syscall in [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_connect,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_pidfd_getfd,
        libc::SYS_kcmp,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_move_mount,
        libc::SYS_fsopen,
        libc::SYS_fsmount,
        libc::SYS_open_tree,
        libc::SYS_mount_setattr,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_open_by_handle_at,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_reboot,
        libc::SYS_kexec_load,
    ] {
        filter.push(instruction(0x15, 0, 1, syscall as u32));
        filter.push(instruction(0x06, 0, 0, 0x0005_0000 | libc::EPERM as u32));
    }
    // clone3 hides its flags behind a pointer; ENOSYS makes libc fall back to
    // clone, whose namespace flags this filter can inspect directly.
    filter.extend([
        instruction(0x15, 0, 1, libc::SYS_clone3 as u32),
        instruction(0x06, 0, 0, 0x0005_0000 | libc::ENOSYS as u32),
        instruction(0x15, 0, 3, libc::SYS_clone as u32),
        instruction(0x20, 0, 0, 16), // args[0], low word on supported little-endian ABIs
        instruction(0x45, 0, 1, 0x7e02_0000), // CLONE_NEW* namespace flags
        instruction(0x06, 0, 0, 0x0005_0000 | libc::EPERM as u32),
        instruction(0x06, 0, 0, 0x7fff_0000), // ALLOW
    ]);
    let program = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_mut_ptr() };
    // SAFETY: the BPF program and backing array outlive prctl, which copies them.
    // Callers set no-new-privileges before installing this inherited filter.
    unsafe { libc::prctl(libc::PR_SET_SECCOMP, 2, &raw const program, 0, 0) == 0 }
}
