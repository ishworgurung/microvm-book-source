// seccomp_thread: a seccomp filter on one thread, the way
// Firecracker gives each of its threads its own filter.
//
//   seccomp_thread          denied calls fail with EPERM
//   seccomp_thread kill     a denied call kills the process
//
// A "vcpu" thread installs a filter that allows only the few
// system calls it needs. It then tries to open a file, which
// a vCPU thread never has to do. The main thread has no
// filter, so the same open works there. Chapter 5 explains
// the output.

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompFilter,
    TargetArch,
};
use std::collections::BTreeMap;
use std::convert::TryFrom;
use std::fs::File;
use std::thread;

const FILE: &str = "/etc/hostname";

// The system calls this thread may make. An empty rule list
// means "allow this call with any arguments". Firecracker's
// lists also check arguments, such as which ioctl is asked.
fn allowed() -> Vec<i64> {
    vec![
        libc::SYS_write, // println!
        libc::SYS_futex, // locks, thread join
        libc::SYS_mmap,  // the allocator
        libc::SYS_munmap,
        libc::SYS_madvise,
        libc::SYS_mprotect,
        libc::SYS_brk,
        libc::SYS_sigaltstack, // thread exit
        libc::SYS_rt_sigprocmask,
        libc::SYS_exit,
        libc::SYS_exit_group,
    ]
}

fn vcpu_filter(kill: bool) -> BpfProgram {
    let rules: BTreeMap<i64, Vec<_>> =
        allowed().into_iter().map(|nr| (nr, vec![])).collect();
    // What happens to a call that is not on the list.
    let mismatch = if kill {
        SeccompAction::KillProcess
    } else {
        SeccompAction::Errno(libc::EPERM as u32)
    };
    let filter = SeccompFilter::new(
        rules,
        mismatch,
        SeccompAction::Allow, // a call on the list
        TargetArch::x86_64,
    )
    .expect("valid filter");
    BpfProgram::try_from(filter).expect("compiles to BPF")
}

fn try_open(who: &str) {
    match File::open(FILE) {
        Ok(_) => println!("{who}: open {FILE}: ok"),
        Err(e) => println!("{who}: open {FILE}: {e}"),
    }
}

fn main() {
    let kill = std::env::args().nth(1).as_deref() == Some("kill");
    // Compile the filter before the thread starts: building
    // it needs no system calls that the filter would deny.
    let prog = vcpu_filter(kill);

    let vcpu = thread::Builder::new()
        .name("vcpu0".into())
        .spawn(move || {
            apply_filter(&prog).expect("install filter");
            let n = prog.len();
            println!("vcpu0: filter on, {n} BPF instructions");
            try_open("vcpu0");
        })
        .expect("spawn");
    vcpu.join().expect("vcpu thread");

    // The main thread installed no filter.
    try_open("main ");
}
