//! Overhead microbenchmarks for the `logi_prof` instrumentation itself.
//!
//! Before instrumenting hot paths always-on (Plan P2), we must know what the instrumentation costs
//! (AGENTS.md §1B "measure first", §1E "know the cost"). These benches answer:
//!
//! - `scope_enter_exit` — the raw per-scope hot-path cost (one `profile_scope!` enter+exit).
//! - `counter_set` — the per-counter hot-path cost.
//! - `frame_with_scopes` — the realistic per-frame unit (N scopes + one `end_frame` drain), swept over N
//!   so the marginal per-scope cost and the fixed `end_frame` cost can be read off.
//!
//! Run with the engine compiled in:  `cargo bench -p logi_prof --features prof`
//! With `prof` off the same harness measures the no-op path (≈ 0) — proof of zero shipping cost.

fn main() {
    divan::main();
}

/// Raw hot-path cost of a single `profile_scope!` enter+exit. `reset()` runs in the (untimed) input
/// setup each iteration so the per-thread sample buffer never grows — we measure the steady-state push,
/// not amortized reallocations.
#[divan::bench]
fn scope_enter_exit(bencher: divan::Bencher) {
    bencher.with_inputs(logi_prof::reset).bench_values(|()| {
        logi_prof::profile_scope!("bench.hot.scope");
    });
}

/// Hot-path cost of recording one counter gauge.
#[divan::bench]
fn counter_set(bencher: divan::Bencher) {
    bencher.with_inputs(logi_prof::reset).bench_values(|()| {
        logi_prof::counter!("bench.hot.counter", 1u64);
    });
}

/// The realistic per-frame unit: `n` scopes then one `end_frame` drain (which aggregates + publishes and,
/// today, allocates the per-frame maps — a cost worth watching). `end_frame` drains the sink each call, so
/// memory stays bounded. Sweeping `n` separates the marginal per-scope cost from the fixed drain cost.
#[divan::bench(args = [1, 8, 64, 256])]
fn frame_with_scopes(n: usize) -> u64 {
    for _ in 0..n {
        logi_prof::profile_scope!("bench.frame.scope");
    }
    logi_prof::end_frame()
}
