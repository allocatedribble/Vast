//! `logi_prof` — in-process instrumentation for the Logi-rs simulation threads.
//!
//! **Why this exists.** The heavy Rust work runs on Rust-owned threads that Unreal did not create — the
//! gameplay sim thread, the Geo stream thread (`LogiGeoStream`), and the `LogiBevyCompute` pool. A Unreal
//! Insights CPU timer is attributed to *the engine thread that emits it*, so foreign threads are
//! structurally invisible to the Insights timeline (today the editor sees `logi_world_tick` as one opaque
//! block and the Geo stream thread not at all). See
//! [`Docs/Research/UE-5.8-Frame-Profiling-and-Rust-Benchmarking.md`].
//!
//! **What it does.** [`profile_scope!`] wraps a *phase / system / batch* boundary (never a per-entity
//! body — volume must stay O(spans), not O(entities)) and records inclusive + exclusive nanoseconds into a
//! per-thread buffer. [`counter!`] records a scalar gauge. Once per frame the **owner thread** calls
//! [`end_frame`], which folds every thread's buffer into one [`ProfileFrame`] keyed by (name, thread
//! class). The FFI layer polls that frame and the C++ bridge re-emits it as Unreal trace counters / `stat`
//! / CSV, so the Rust timings land on the same frame-aligned Insights timeline as the game/render/GPU
//! threads.
//!
//! **Cost when off.** With the `prof` feature disabled, the macros expand to a no-op guard and the
//! recording engine is not compiled in — zero shipping cost.
//!
//! **Determinism.** Recording is side-effect-free w.r.t. ECS `World` state and is compiled out of
//! golden/replay runs, so it cannot perturb the fixed-step sim (ADR-0001 / ADR-0012).
//!
//! **Threading.** The hot path touches only thread-local state plus one short, *uncontended* per-thread
//! mutex (its sink). The owner's [`end_frame`] is the single consumer; it briefly locks each sink to drain
//! it. This is the same publish/retire discipline as `logi_runtime`'s `SnapshotRing`.
//!
//! Pure safe Rust, std only, no third-party dependencies.
#![forbid(unsafe_code)]

use core::sync::atomic::AtomicU32;

// ----------------------------------------------------------------------------------------------------
// Always-compiled public surface (data types + the callsite cache). The recording *engine* lives in the
// `imp` module behind `#[cfg(feature = "prof")]`; the no-op shims below stand in when it is off.
// ----------------------------------------------------------------------------------------------------

/// Which Rust-owned thread a span/counter was recorded on. Mirrors the FFI `thread_class` byte so the C++
/// bridge can name the track (`Sim` / `GeoStream` / `Worker`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum ThreadClass {
    /// The Unreal game thread, inside an FFI call (e.g. snapshot apply, `set_view`).
    GameFfi = 0,
    /// The gameplay fixed-step sim thread (ADR-0012).
    Sim = 1,
    /// The Geo streaming thread that owns the Geo `bevy_ecs` World (`LogiGeoStream`).
    GeoStream = 2,
    /// A `ComputeTaskPool` worker (`LogiBevyCompute`) running a `par_iter` batch.
    Worker = 3,
    /// Unclassified (the thread never called [`register_thread_class`]).
    Other = 255,
}

impl ThreadClass {
    /// Map a raw FFI byte back to a class (unknown values fold to [`ThreadClass::Other`]).
    #[inline]
    #[must_use]
    pub fn from_u8(v: u8) -> Self {
        match v {
            0 => Self::GameFfi,
            1 => Self::Sim,
            2 => Self::GeoStream,
            3 => Self::Worker,
            _ => Self::Other,
        }
    }
}

/// A per-callsite cache of the interned name id. Constructed once per `profile_scope!` / `counter!` site as
/// a function-local `static`; the first call interns the name and stores the dense id here, so every later
/// call is a single relaxed atomic load (no map lookup, no lock).
// The cached id is only *read* by the recording engine; when `prof` is off the field is intentionally
// inert, so silence the false-positive dead-code lint in that configuration only.
pub struct CallsiteId(#[cfg_attr(not(feature = "prof"), allow(dead_code))] AtomicU32);

impl CallsiteId {
    /// Sentinel-initialised (`u32::MAX` = "not yet interned"). `const` so it can live in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self(AtomicU32::new(u32::MAX))
    }
}

impl Default for CallsiteId {
    fn default() -> Self {
        Self::new()
    }
}

/// One aggregated span in a [`ProfileFrame`]: a logical name on one thread class, summed over all calls
/// that frame.
#[derive(Clone, Copy, Debug)]
pub struct SpanReport {
    /// The interned `&'static str` name passed to [`profile_scope!`].
    pub name: &'static str,
    /// Which thread class recorded it.
    pub thread_class: ThreadClass,
    /// Number of times the scope was entered this frame.
    pub count: u32,
    /// Total wall time inside the scope (including children), summed over `count`.
    pub inclusive_ns: u64,
    /// Total wall time inside the scope *excluding* nested `profile_scope!` children.
    pub exclusive_ns: u64,
    /// Largest single inclusive time observed this frame (worst-case spike).
    pub max_ns: u64,
}

/// One scalar gauge in a [`ProfileFrame`] (last write of the frame wins).
#[derive(Clone, Copy, Debug)]
pub struct CounterReport {
    /// The interned `&'static str` name passed to [`counter!`].
    pub name: &'static str,
    /// The value (gauge semantics: the last value recorded this frame).
    pub value: f64,
}

/// The published per-frame profile. Built by [`end_frame`], readable via [`latest`]. Output order is
/// deterministic (sorted) so consumers and golden tests see a stable layout (§8 discipline).
#[derive(Clone, Debug)]
pub struct ProfileFrame {
    /// Monotonic id assigned by [`end_frame`].
    pub frame_id: u64,
    /// Wall time the owner thread spent between the previous and this `end_frame` (the frame budget).
    pub owner_wall_ns: u64,
    /// Aggregated spans, sorted by (thread class, name).
    pub spans: Vec<SpanReport>,
    /// Aggregated counters, sorted by name.
    pub counters: Vec<CounterReport>,
}

/// Convert common numeric types to the `f64` a counter stores, without a redundant cast at the call site
/// (so `counter!("x", 5.0)` and `counter!("n", len_usize)` both lint-cleanly under `-D warnings`).
pub trait IntoCounterValue {
    /// The gauge value.
    fn into_counter_value(self) -> f64;
}
macro_rules! into_counter_value_via_cast {
    ($($t:ty),*) => {$(
        impl IntoCounterValue for $t {
            #[inline]
            fn into_counter_value(self) -> f64 { self as f64 }
        }
    )*};
}
into_counter_value_via_cast!(f32, i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
impl IntoCounterValue for f64 {
    #[inline]
    fn into_counter_value(self) -> f64 {
        self
    }
}

// ----------------------------------------------------------------------------------------------------
// Macros (always exported; the work behind them is gated by `prof`).
// ----------------------------------------------------------------------------------------------------

/// Time the enclosing scope under a `&'static str` name. RAII: starts on entry, records on drop.
///
/// Use at a **phase / system / batch** boundary, never per entity. The name is interned once per callsite.
///
/// ```
/// fn decode_batch() {
///     logi_prof::profile_scope!("geo.decode");
///     // ... work ...
/// }
/// ```
#[macro_export]
macro_rules! profile_scope {
    ($name:expr) => {
        // The `static` lives in its own block so multiple `profile_scope!`s in one function don't collide;
        // the guard it returns is bound in the *outer* scope (drops at the enclosing scope's end).
        let _logi_prof_guard = {
            static __LOGI_PROF_ID: $crate::CallsiteId = $crate::CallsiteId::new();
            $crate::ScopeGuard::new(&__LOGI_PROF_ID, $name)
        };
    };
}

/// Record a scalar gauge (last value of the frame wins). Accepts any common numeric type.
///
/// ```
/// # let resident = 42usize;
/// logi_prof::counter!("geo.resident_tiles", resident);
/// ```
#[macro_export]
macro_rules! counter {
    ($name:expr, $value:expr) => {{
        static __LOGI_PROF_CID: $crate::CallsiteId = $crate::CallsiteId::new();
        $crate::record_counter(
            &__LOGI_PROF_CID,
            $name,
            $crate::IntoCounterValue::into_counter_value($value),
        );
    }};
}

// ----------------------------------------------------------------------------------------------------
// `prof` ENABLED — the recording engine.
// ----------------------------------------------------------------------------------------------------

#[cfg(feature = "prof")]
mod imp {
    use super::{CallsiteId, CounterReport, ProfileFrame, SpanReport, ThreadClass};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex, OnceLock, RwLock};
    use std::time::Instant;

    // ---- name interner: &'static str <-> dense u32 id (cold path, behind one mutex) ----------------
    #[derive(Default)]
    struct Interner {
        map: HashMap<&'static str, u32>,
        names: Vec<&'static str>,
    }
    fn interner() -> &'static Mutex<Interner> {
        static I: OnceLock<Mutex<Interner>> = OnceLock::new();
        I.get_or_init(|| Mutex::new(Interner::default()))
    }
    pub fn intern(cache: &CallsiteId, name: &'static str) -> u32 {
        let cached = cache.0.load(Ordering::Relaxed);
        if cached != u32::MAX {
            return cached;
        }
        let mut it = interner().lock().unwrap();
        let id = match it.map.get(name) {
            Some(&id) => id,
            None => {
                let id = it.names.len() as u32;
                it.names.push(name);
                it.map.insert(name, id);
                id
            }
        };
        cache.0.store(id, Ordering::Relaxed);
        id
    }
    fn name_of(id: u32) -> &'static str {
        interner()
            .lock()
            .unwrap()
            .names
            .get(id as usize)
            .copied()
            .unwrap_or("<unknown>")
    }

    // ---- per-thread sink ---------------------------------------------------------------------------
    struct Sample {
        id: u32,
        inclusive_ns: u64,
        exclusive_ns: u64,
    }
    struct ThreadSink {
        class: AtomicU8,
        samples: Mutex<Vec<Sample>>,
        counters: Mutex<Vec<(u32, f64)>>,
    }
    impl ThreadSink {
        fn new() -> Self {
            Self {
                class: AtomicU8::new(ThreadClass::Other as u8),
                samples: Mutex::new(Vec::with_capacity(256)),
                counters: Mutex::new(Vec::with_capacity(64)),
            }
        }
    }
    fn registry() -> &'static Mutex<Vec<Arc<ThreadSink>>> {
        static R: OnceLock<Mutex<Vec<Arc<ThreadSink>>>> = OnceLock::new();
        R.get_or_init(|| Mutex::new(Vec::new()))
    }

    struct ScopeFrame {
        id: u32,
        start: Instant,
        child_ns: u64,
    }
    struct Tls {
        stack: Vec<ScopeFrame>,
        sink: Arc<ThreadSink>,
    }
    thread_local! {
        static TLS: RefCell<Tls> = {
            let sink = Arc::new(ThreadSink::new());
            registry().lock().unwrap().push(Arc::clone(&sink));
            RefCell::new(Tls { stack: Vec::with_capacity(32), sink })
        };
    }

    pub fn register_thread_class(class: ThreadClass) {
        TLS.with(|t| t.borrow().sink.class.store(class as u8, Ordering::Relaxed));
    }

    pub fn enter_scope(id: u32) {
        TLS.with(|t| {
            t.borrow_mut().stack.push(ScopeFrame {
                id,
                start: Instant::now(),
                child_ns: 0,
            });
        });
    }

    pub fn exit_scope() {
        TLS.with(|t| {
            let mut tb = t.borrow_mut();
            let Some(frame) = tb.stack.pop() else {
                return;
            };
            let inclusive = frame.start.elapsed().as_nanos() as u64;
            let exclusive = inclusive.saturating_sub(frame.child_ns);
            if let Some(parent) = tb.stack.last_mut() {
                parent.child_ns += inclusive;
            }
            tb.sink.samples.lock().unwrap().push(Sample {
                id: frame.id,
                inclusive_ns: inclusive,
                exclusive_ns: exclusive,
            });
        });
    }

    pub fn record_counter(id: u32, value: f64) {
        TLS.with(|t| t.borrow().sink.counters.lock().unwrap().push((id, value)));
    }

    // ---- per-frame publish (owner thread, single consumer) -----------------------------------------
    fn latest_cell() -> &'static RwLock<Option<Arc<ProfileFrame>>> {
        static L: OnceLock<RwLock<Option<Arc<ProfileFrame>>>> = OnceLock::new();
        L.get_or_init(|| RwLock::new(None))
    }

    fn next_frame_id() -> u64 {
        static F: AtomicU64 = AtomicU64::new(0);
        F.fetch_add(1, Ordering::Relaxed)
    }

    fn owner_wall_ns() -> u64 {
        static LAST: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
        let m = LAST.get_or_init(|| Mutex::new(None));
        let mut g = m.lock().unwrap();
        let now = Instant::now();
        let wall = g.map_or(0, |prev| now.duration_since(prev).as_nanos() as u64);
        *g = Some(now);
        wall
    }

    #[derive(Default, Clone, Copy)]
    struct SpanAgg {
        count: u32,
        inclusive_ns: u64,
        exclusive_ns: u64,
        max_ns: u64,
    }

    pub fn end_frame() -> u64 {
        let mut spans: HashMap<(u32, u8), SpanAgg> = HashMap::new();
        let mut counters: HashMap<u32, f64> = HashMap::new();
        {
            let reg = registry().lock().unwrap();
            for sink in reg.iter() {
                let class = sink.class.load(Ordering::Relaxed);
                for s in sink.samples.lock().unwrap().drain(..) {
                    let e = spans.entry((s.id, class)).or_default();
                    e.count += 1;
                    e.inclusive_ns += s.inclusive_ns;
                    e.exclusive_ns += s.exclusive_ns;
                    e.max_ns = e.max_ns.max(s.inclusive_ns);
                }
                for (id, v) in sink.counters.lock().unwrap().drain(..) {
                    counters.insert(id, v); // gauge: last write wins
                }
            }
        }

        let mut span_vec: Vec<SpanReport> = spans
            .into_iter()
            .map(|((id, class), a)| SpanReport {
                name: name_of(id),
                thread_class: ThreadClass::from_u8(class),
                count: a.count,
                inclusive_ns: a.inclusive_ns,
                exclusive_ns: a.exclusive_ns,
                max_ns: a.max_ns,
            })
            .collect();
        span_vec.sort_by(|a, b| (a.thread_class, a.name).cmp(&(b.thread_class, b.name)));

        let mut counter_vec: Vec<CounterReport> = counters
            .into_iter()
            .map(|(id, value)| CounterReport {
                name: name_of(id),
                value,
            })
            .collect();
        counter_vec.sort_by(|a, b| a.name.cmp(b.name));

        let frame_id = next_frame_id();
        let frame = Arc::new(ProfileFrame {
            frame_id,
            owner_wall_ns: owner_wall_ns(),
            spans: span_vec,
            counters: counter_vec,
        });
        *latest_cell().write().unwrap() = Some(frame);
        frame_id
    }

    pub fn latest() -> Option<Arc<ProfileFrame>> {
        latest_cell().read().unwrap().clone()
    }

    /// Drain every sink and clear the latest frame — test isolation only.
    pub fn reset() {
        for sink in registry().lock().unwrap().iter() {
            sink.samples.lock().unwrap().clear();
            sink.counters.lock().unwrap().clear();
        }
        *latest_cell().write().unwrap() = None;
    }
}

// ----------------------------------------------------------------------------------------------------
// `prof` ENABLED — thin public wrappers + the RAII guard.
// ----------------------------------------------------------------------------------------------------

#[cfg(feature = "prof")]
mod enabled_api {
    use super::{CallsiteId, ThreadClass};

    /// RAII guard returned by [`profile_scope!`](crate::profile_scope). Records the scope on drop.
    pub struct ScopeGuard {
        _private: (),
    }
    impl ScopeGuard {
        /// Intern the name (cheap after the first call) and open a scope on the current thread.
        #[inline]
        #[must_use]
        pub fn new(id: &'static CallsiteId, name: &'static str) -> Self {
            let interned = super::imp::intern(id, name);
            super::imp::enter_scope(interned);
            Self { _private: () }
        }
    }
    impl Drop for ScopeGuard {
        #[inline]
        fn drop(&mut self) {
            super::imp::exit_scope();
        }
    }

    /// Tag the current thread so its spans/counters carry the right class. Call once at thread start.
    #[inline]
    pub fn register_thread_class(class: ThreadClass) {
        super::imp::register_thread_class(class);
    }

    /// Backing fn for [`counter!`](crate::counter). Prefer the macro.
    #[inline]
    pub fn record_counter(id: &'static CallsiteId, name: &'static str, value: f64) {
        let interned = super::imp::intern(id, name);
        super::imp::record_counter(interned, value);
    }

    /// Fold every thread's buffer into a [`ProfileFrame`](crate::ProfileFrame) and publish it. The owner
    /// thread calls this once per frame. Returns the new frame id.
    #[inline]
    pub fn end_frame() -> u64 {
        super::imp::end_frame()
    }

    /// The most recently published frame, if any.
    #[inline]
    #[must_use]
    pub fn latest() -> Option<std::sync::Arc<super::ProfileFrame>> {
        super::imp::latest()
    }

    /// Drain all buffers and clear the latest frame. Test isolation only.
    pub fn reset() {
        super::imp::reset();
    }
}

#[cfg(feature = "prof")]
pub use enabled_api::{end_frame, latest, record_counter, register_thread_class, reset, ScopeGuard};

// ----------------------------------------------------------------------------------------------------
// `prof` DISABLED — zero-cost no-op shims. The engine is not compiled.
// ----------------------------------------------------------------------------------------------------

#[cfg(not(feature = "prof"))]
mod disabled_api {
    use super::{CallsiteId, ProfileFrame, ThreadClass};
    use std::sync::Arc;

    /// No-op stand-in for the enabled [`ScopeGuard`]; a ZST with no `Drop` — fully elided.
    pub struct ScopeGuard;
    impl ScopeGuard {
        #[inline(always)]
        #[must_use]
        pub fn new(_id: &'static CallsiteId, _name: &'static str) -> Self {
            Self
        }
    }

    #[inline(always)]
    pub fn register_thread_class(_class: ThreadClass) {}

    #[inline(always)]
    pub fn record_counter(_id: &'static CallsiteId, _name: &'static str, _value: f64) {}

    #[inline(always)]
    pub fn end_frame() -> u64 {
        0
    }

    #[inline(always)]
    #[must_use]
    pub fn latest() -> Option<Arc<ProfileFrame>> {
        None
    }

    #[inline(always)]
    pub fn reset() {}
}

#[cfg(not(feature = "prof"))]
pub use disabled_api::{end_frame, latest, record_counter, register_thread_class, reset, ScopeGuard};

// ----------------------------------------------------------------------------------------------------
// Tests (require the engine — run with `--features prof`). Stateful checks are confined to ONE test so the
// process-global profiler is never exercised by two tests at once.
// ----------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "prof"))]
mod tests {
    use super::*;
    use std::thread;

    /// Spin for at least `ns` nanoseconds without sleeping (sleep is too coarse/edgy for sub-µs asserts;
    /// we only assert ordering relationships, never absolute timings).
    fn busy_for(ns: u64) {
        let start = std::time::Instant::now();
        while (start.elapsed().as_nanos() as u64) < ns {
            std::hint::spin_loop();
        }
    }

    #[test]
    fn callsite_interning_is_stable_and_per_name() {
        let a = CallsiteId::new();
        let b = CallsiteId::new();
        // Same callsite, same name -> same id, twice.
        let id1 = imp::intern(&a, "dup.name");
        let id2 = imp::intern(&a, "dup.name");
        assert_eq!(id1, id2);
        // A different callsite with the SAME name folds to the same id (aggregate by logical name).
        let id3 = imp::intern(&b, "dup.name");
        assert_eq!(id1, id3);
        // A different name gets a different id.
        let other = CallsiteId::new();
        assert_ne!(imp::intern(&other, "other.name"), id1);
    }

    #[test]
    fn profiler_end_to_end() {
        reset();
        register_thread_class(ThreadClass::Sim);

        // Nested scopes: parent contains a child + its own exclusive work.
        {
            profile_scope!("test.parent");
            busy_for(200_000); // 200µs of own work
            {
                profile_scope!("test.child");
                busy_for(300_000); // 300µs inside the child
            }
            counter!("test.gauge", 7u64);
        }

        // A worker thread records the same logical name on a different class.
        let h = thread::spawn(|| {
            register_thread_class(ThreadClass::Worker);
            profile_scope!("test.parallel");
            busy_for(100_000);
        });
        h.join().unwrap();

        let id = end_frame();
        let frame = latest().expect("a frame was published");
        assert_eq!(frame.frame_id, id);

        let span = |name: &str, class: ThreadClass| {
            frame
                .spans
                .iter()
                .find(|s| s.name == name && s.thread_class == class)
                .copied()
        };

        let parent = span("test.parent", ThreadClass::Sim).expect("parent recorded");
        let child = span("test.child", ThreadClass::Sim).expect("child recorded");
        assert_eq!(parent.count, 1);
        assert_eq!(child.count, 1);
        // Inclusive parent time covers the child; exclusive strips it. (Loose bounds — timing-robust.)
        assert!(parent.inclusive_ns >= child.inclusive_ns, "parent encloses child");
        assert!(
            parent.exclusive_ns < parent.inclusive_ns,
            "exclusive ({}) strips the child from inclusive ({})",
            parent.exclusive_ns,
            parent.inclusive_ns
        );

        // The worker-thread span is attributed to the Worker class, separate from the Sim spans.
        assert!(
            span("test.parallel", ThreadClass::Worker).is_some(),
            "worker span on its own class"
        );
        assert!(
            span("test.parallel", ThreadClass::Sim).is_none(),
            "not misattributed to Sim"
        );

        // Counter round-trips as a gauge.
        let gauge = frame
            .counters
            .iter()
            .find(|c| c.name == "test.gauge")
            .expect("counter recorded");
        assert_eq!(gauge.value, 7.0);

        // Output order is deterministic (sorted by class then name).
        let mut sorted = frame.spans.clone();
        sorted.sort_by(|a, b| (a.thread_class, a.name).cmp(&(b.thread_class, b.name)));
        let original: Vec<_> = frame.spans.iter().map(|s| (s.thread_class, s.name)).collect();
        let resorted: Vec<_> = sorted.iter().map(|s| (s.thread_class, s.name)).collect();
        assert_eq!(original, resorted, "spans are emitted in deterministic sorted order");

        // A second end_frame with no activity drains cleanly to an empty span set.
        let _ = end_frame();
        let empty = latest().unwrap();
        assert!(empty.spans.is_empty(), "buffers were drained, next frame is empty");
    }

    #[test]
    fn counter_accepts_common_numeric_types() {
        // Compile-test: the IntoCounterValue trait covers the integer + float widths we use.
        assert_eq!(IntoCounterValue::into_counter_value(5u64), 5.0);
        assert_eq!(IntoCounterValue::into_counter_value(5usize), 5.0);
        assert_eq!(IntoCounterValue::into_counter_value(2.5f32), 2.5);
        assert_eq!(IntoCounterValue::into_counter_value(-3i32), -3.0);
        assert_eq!(IntoCounterValue::into_counter_value(1.5f64), 1.5);
    }
}
