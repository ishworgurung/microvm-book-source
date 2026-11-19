// lazy_restore: two ways to load a snapshot's memory file.
//
//   lazy_restore [MiB]      default 64
//
// It writes a "memory file" of MiB mebibytes, then "restores"
// it twice and touches a scattered 5% of its pages, like a
// restored guest that runs one small job:
//
//   eager  read() the whole file into memory, then run
//   lazy   mmap(MAP_PRIVATE) the file and run at once; each
//          page is loaded on the first touch (Firecracker's
//          File backend works this way)
//
// For each it prints when the "guest" could start, when it
// had touched its pages, and how many page faults that took.
// Finally it writes to a mapped page and shows that the file
// on disk did not change: MAP_PRIVATE copies on write.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::io::AsRawFd;
use std::time::Instant;

const PAGE: usize = 4096;

// Minor page faults of this process so far.
fn minor_faults() -> i64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    ru.ru_minflt
}

// The pages a small job touches: 5% of them, in a scattered
// order. Knuth's multiplicative hash visits every page once
// when the page count is a power of two.
fn touched(npages: usize) -> Vec<usize> {
    (0..npages / 20)
        .map(|t| (t * 2_654_435_761) % npages)
        .collect()
}

// Read one byte from each touched page, as a guest would.
fn run_guest(mem: &[u8], pages: &[usize]) -> u64 {
    pages.iter().map(|&p| mem[p * PAGE] as u64).sum()
}

// One line of results: when the guest could start, when it
// had touched its pages, and the page faults that took.
fn report(what: &str, ready: f64, done: f64, faults: i64) {
    println!(
        "{what}: ready after {ready:6.2} ms, \
         done after {done:6.2} ms, {faults} faults"
    );
}

fn ms(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

fn main() -> std::io::Result<()> {
    let mib: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let len = mib << 20;
    let npages = len / PAGE;
    let pages = touched(npages);
    let path = std::env::temp_dir().join("ch05-vm.mem");

    // 1. The "snapshot": every page starts with its number.
    let mut f = File::create(&path)?;
    let mut page = vec![0u8; PAGE];
    for p in 0..npages {
        page[0] = p as u8;
        f.write_all(&page)?;
    }
    f.sync_all()?;
    let n = pages.len();
    println!("file: {mib} MiB, {npages} pages, {n} touched");

    // 2. Eager: copy everything in, then run.
    let t = Instant::now();
    let f0 = minor_faults();
    let mut buf = Vec::with_capacity(len);
    File::open(&path)?.read_to_end(&mut buf)?;
    let ready = ms(t);
    let sum_e = run_guest(&buf, &pages);
    report("eager", ready, ms(t), minor_faults() - f0);
    drop(buf);

    // 3. Lazy: map the file and run at once.
    let t = Instant::now();
    let f0 = minor_faults();
    let file = File::open(&path)?;
    let mem = unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        assert_ne!(p, libc::MAP_FAILED, "mmap failed");
        std::slice::from_raw_parts_mut(p as *mut u8, len)
    };
    let ready = ms(t);
    let sum_l = run_guest(mem, &pages);
    report("lazy ", ready, ms(t), minor_faults() - f0);
    assert_eq!(sum_e, sum_l, "both restores see the same data");

    // 4. A guest write lands in a private copy of the page.
    mem[0] = 0xAA;
    let on_disk = fs::read(&path)?[0];
    let in_mem = mem[0];
    println!("page 0 after a write of 0xAA:");
    println!(
        "  in memory {in_mem:#04x}, in the file {on_disk:#04x}"
    );

    unsafe { libc::munmap(mem.as_mut_ptr() as *mut _, len) };
    fs::remove_file(&path)?;
    Ok(())
}
