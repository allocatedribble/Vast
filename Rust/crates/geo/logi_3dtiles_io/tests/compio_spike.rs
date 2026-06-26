//! Exp B (spike) — the ADR-0014 I/O-lane substrate: compio's completion **proactor**, driven from a manual
//! frame-harvest loop with **no async executor**, and its owned-buffer cancellation-safety. Windows / IOCP
//! (the platform-now; the same `Proactor` API is io_uring on Linux later). Dev-dep only; never shipped.
//!
//! Validates the two architecturally-load-bearing claims for the runtime's I/O lane:
//!  1. **Drive without an executor** — the proactor runs in exactly the `push -> poll(frame_budget) -> pop`
//!     shape the design's per-frame poll wants (§3.5). No `block_on`, no runtime — just the driver.
//!  2. **Owned-buffer cancellation safety** — "no post-cancel buffer access" is an *ownership* guarantee:
//!     the buffer is moved into the op at `push`; `cancel` either hands it back (op already done) or consumes
//!     the key and the driver retains the buffer until the kernel finishes. Neither path lets the user touch
//!     a buffer the kernel may still be writing (the completion-I/O safety rule the design calls a HARD GATE).
//!
//! Run: cargo test -p logi_3dtiles_io --features cyper-h2 --test compio_spike -- --nocapture
//!
//! Feature-gated on `cyper-h2`: compio is an optional dep pulled only by that feature.
#![cfg(feature = "cyper-h2")]

#[cfg(windows)]
#[test]
fn compio_proactor_frame_harvest_and_owned_buffer_cancel() {
    use compio::driver::op::{Asyncify, CloseFile, ReadAt};
    use compio::driver::{AsRawFd, OpCode, OwnedFd, Proactor, PushEntry, SharedFd};
    use compio::BufResult;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{FromRawHandle, IntoRawHandle, OwnedHandle};
    use std::time::{Duration, Instant};

    // FILE_FLAG_OVERLAPPED — so the file handle does true async (completion) I/O through the proactor.
    const FILE_FLAG_OVERLAPPED: u32 = 0x4000_0000;

    /// Blocking submit-and-wait for the setup ops (open/close), straight from compio's `driver.rs` example.
    fn push_and_wait<O: OpCode + 'static>(driver: &mut Proactor, op: O) -> (usize, O) {
        match driver.push(op) {
            PushEntry::Ready(res) => res.unwrap(),
            PushEntry::Pending(mut key) => loop {
                driver.poll(None).unwrap();
                match driver.pop(key) {
                    PushEntry::Pending(k) => key = k,
                    PushEntry::Ready(res) => break res.unwrap(),
                }
            },
        }
    }

    let path = std::env::temp_dir().join("logi_compio_spike.bin");
    let file_len = 1usize << 20; // 1 MiB to read concurrently from many offsets
    std::fs::write(&path, vec![0xABu8; file_len]).unwrap();

    let mut driver = Proactor::new().unwrap();

    // Open the file overlapped, on the proactor's blocking pool (the example's pattern).
    let fd = {
        let p = path.clone();
        let open = Asyncify::new(move || {
            BufResult(
                std::fs::OpenOptions::new()
                    .read(true)
                    .attributes(FILE_FLAG_OVERLAPPED)
                    .open(&p)
                    .map(|f| f.into_raw_handle() as usize),
                (),
            )
        });
        let (h, _) = push_and_wait(&mut driver, open);
        OwnedFd::File(unsafe { OwnedHandle::from_raw_handle(h as _) })
    };
    let fd = SharedFd::new(fd);
    driver.attach(fd.as_raw_fd()).unwrap();

    // ---- Claim 1: frame-harvest with NO executor — push N reads, poll(budget), drain completions. ----
    let chunk = 4096usize;
    let n = 128usize;
    let mut pending = Vec::new();
    let mut harvested = 0usize;
    for i in 0..n {
        let op = ReadAt::new(fd.clone(), (i * chunk) as u64, Vec::<u8>::with_capacity(chunk));
        match driver.push(op) {
            PushEntry::Ready(res) => {
                let _ = res.unwrap();
                harvested += 1;
            }
            PushEntry::Pending(key) => pending.push(key),
        }
    }
    let t0 = Instant::now();
    let mut polls = 0usize;
    while harvested < n {
        driver.poll(Some(Duration::from_millis(16))).unwrap(); // the per-frame budget
        polls += 1;
        let mut still = Vec::with_capacity(pending.len());
        for key in pending.drain(..) {
            match driver.pop(key) {
                PushEntry::Ready(res) => {
                    let _ = res.unwrap();
                    harvested += 1;
                }
                PushEntry::Pending(k) => still.push(k),
            }
        }
        pending = still;
        assert!(polls < 1000, "harvest did not converge");
    }
    let harvest_time = t0.elapsed();
    assert_eq!(harvested, n, "all reads harvested");
    eprintln!(
        "\n=== Exp B: compio proactor (IOCP), NO async executor ===\n  frame-harvest: {n} concurrent reads drained in {harvest_time:.2?} over {polls} poll(16ms) call(s)"
    );

    // ---- Claim 2: owned-buffer cancellation safety. The buffer is moved into the op at `push`, so after
    //      `cancel` it is NEVER user-accessible: either `cancel` returns it (op completed) or it consumes the
    //      key and the driver retains the buffer until the kernel finishes. We can't alias a kernel-owned
    //      buffer in either case. ----
    let mut returned_by_cancel = 0usize; // op completed → buffer handed back via cancel
    let mut driver_retained = 0usize; // in-flight → key consumed, buffer driver-owned (unreachable to us)
    for i in 0..32usize {
        let op = ReadAt::new(fd.clone(), (i * chunk) as u64, Vec::<u8>::with_capacity(chunk));
        match driver.push(op) {
            PushEntry::Ready(res) => {
                let _ = res.unwrap(); // completed synchronously; buffer is in the result, never aliased
            }
            PushEntry::Pending(key) => match driver.cancel(key) {
                Some(BufResult(_res, _buf)) => returned_by_cancel += 1,
                None => driver_retained += 1,
            },
        }
    }
    // Let any in-flight cancellations settle so the driver reclaims their buffers before it drops.
    for _ in 0..16 {
        driver.poll(Some(Duration::from_millis(2))).ok();
    }
    eprintln!(
        "  cancellation: {returned_by_cancel} ops returned their buffer on cancel, {driver_retained} were in-flight (driver-owned until the kernel finishes)\n  → no post-cancel buffer access is expressible — the completion-I/O HARD GATE holds by construction.\n"
    );

    // Close the file through the proactor too (best-effort — skipped if a cancelled op still holds a clone).
    if let Ok(raw) = fd.try_unwrap() {
        push_and_wait(&mut driver, CloseFile::new(raw));
    }
    let _ = std::fs::remove_file(&path);
}
