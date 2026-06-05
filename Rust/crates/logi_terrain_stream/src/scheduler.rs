//! The streaming scheduler (scheduler-design.md §6): page lifecycle, queues, a **transparent** weighted
//! priority (every term recorded), budgets, and the inputs for predictive streaming.

use logi_terrain_core::{DVec3, TileId};
use logi_terrain_lod::screen_space_error;

/// Scheduling queues (§6.2), most urgent first.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(usize)]
pub enum QueueClass {
    CriticalNow = 0,
    NextFrame = 1,
    NearFuture = 2,
    Speculative = 3,
    Maintenance = 4,
    Eviction = 5,
}

pub const QUEUE_COUNT: usize = 6;

/// Budgets Rust owns (§6.4). Unity passes device class + frame timing; Rust decides.
#[derive(Clone, Copy, Debug)]
pub struct StreamingBudgets {
    /// FIX L3: the EXACT cache byte budget this set was derived from. `total_cpu_cache_mb` is the
    /// MB-rounded view for display/derived sub-budgets; this keeps `set_budgets(budgets())` idempotent
    /// (re-deriving the cache size from the rounded MB field would floor a sub-MB budget to 0/1 MB).
    pub total_cpu_cache_bytes: u64,
    pub total_cpu_cache_mb: u32,
    pub compressed_cache_mb: u32,
    pub decoded_cache_mb: u32,
    pub gpu_mirror_target_mb: u32,
    pub network_bytes_per_sec: u64,
    pub disk_bytes_per_sec: u64,
    pub decode_ms_per_frame: f32,
    pub upload_bytes_per_frame: u64,
    pub max_inflight_requests: u32,
    pub max_provider_requests_per_host: u32,
    pub max_pages_promoted_per_frame: u32,
    pub max_pages_evicted_per_frame: u32,
    pub max_procedural_batches_per_frame: u32,
    pub collision_memory_mb: u32,
}

impl StreamingBudgets {
    /// Derive a default budget set from the CPU cache size + in-flight cap.
    pub fn from_cache(total_cpu_cache_bytes: usize, max_inflight: u32) -> Self {
        let mb = (total_cpu_cache_bytes / (1024 * 1024)).max(1) as u32;
        Self {
            total_cpu_cache_bytes: total_cpu_cache_bytes as u64, // FIX L3: keep exact bytes for idempotent re-apply
            total_cpu_cache_mb: mb,
            compressed_cache_mb: mb / 2,
            decoded_cache_mb: mb,
            gpu_mirror_target_mb: mb,
            network_bytes_per_sec: 32 * 1024 * 1024,
            disk_bytes_per_sec: 512 * 1024 * 1024,
            decode_ms_per_frame: 4.0,
            upload_bytes_per_frame: 16 * 1024 * 1024,
            max_inflight_requests: max_inflight,
            max_provider_requests_per_host: max_inflight,
            max_pages_promoted_per_frame: 128,
            max_pages_evicted_per_frame: 128,
            max_procedural_batches_per_frame: 8,
            collision_memory_mb: mb / 8,
        }
    }
}

/// The transparent weighted priority (§6.3) — every term is recorded so a late load is explainable.
#[derive(Clone, Copy, Debug, Default)]
pub struct PriorityTerms {
    pub visibility_score: f32,
    pub screen_error_score: f32,
    pub gameplay_importance: f32,
    pub camera_velocity_prefetch: f32,
    pub route_or_cursor_interest: f32,
    pub parent_fallback_risk: f32,
    pub latency_compensation: f32,
    pub procedural_dependency_score: f32,
    pub memory_cost: f32,
    pub decode_cost: f32,
    pub provider_penalty: f32,
}

impl PriorityTerms {
    pub fn total(&self) -> f32 {
        self.visibility_score
            + self.screen_error_score
            + self.gameplay_importance
            + self.camera_velocity_prefetch
            + self.route_or_cursor_interest
            + self.parent_fallback_risk
            + self.latency_compensation
            + self.procedural_dependency_score
            - self.memory_cost
            - self.decode_cost
            - self.provider_penalty
    }
}

/// Inputs for one priority evaluation. The §6.5 prediction signals plug in here as they become available.
pub struct EvalContext {
    pub camera: DVec3,
    pub motion: DVec3,
    pub fov_y_radians: f64,
    pub viewport_height: f64,
    pub base_geometric_error: f64,
}

/// Evaluate a candidate tile's priority. `is_fallback` = no resident ancestor yet covers the leaf
/// (a visible hole / missing parent → critical).
pub fn evaluate(
    ctx: &EvalContext,
    center_x: f64,
    center_z: f64,
    candidate: TileId,
    leaf: TileId,
    is_fallback: bool,
) -> PriorityTerms {
    let dx = center_x - ctx.camera.x;
    let dz = center_z - ctx.camera.z;
    let dist = (dx * dx + dz * dz).sqrt().max(1.0);

    let visibility_score = (4000.0 / (dist + 1.0)) as f32;

    // FIX L1: overflow-safe geometric-error halving (matches logi_terrain_lod FIX 5) — `1u64 << level`
    // would overflow past level 63; `0.5^level` cannot.
    let ge = ctx.base_geometric_error * 0.5f64.powi(candidate.level as i32);
    let screen_error_score =
        screen_space_error(ge, dist, ctx.fov_y_radians, ctx.viewport_height).min(64.0) as f32;

    // Coarser ancestors covering a fallback hole rank up (preserves coarse-first / parent fallback).
    let parent_fallback_risk = if is_fallback {
        4.0 + leaf.level.saturating_sub(candidate.level) as f32 * 2.0
    } else {
        0.0
    };

    // Prefetch in the direction of motion (velocity cone).
    let mlen = (ctx.motion.x * ctx.motion.x + ctx.motion.z * ctx.motion.z).sqrt();
    let camera_velocity_prefetch = if mlen > 1e-6 {
        let dot = (dx * ctx.motion.x + dz * ctx.motion.z) / (dist * mlen);
        (dot.max(0.0) * 3.0) as f32
    } else {
        0.0
    };

    PriorityTerms {
        visibility_score,
        screen_error_score,
        camera_velocity_prefetch,
        parent_fallback_risk,
        memory_cost: 0.5,
        decode_cost: 0.25,
        ..Default::default()
    }
}

/// Route a candidate to a queue (§6.2) from its terms.
pub fn classify_queue(terms: &PriorityTerms, is_fallback: bool) -> QueueClass {
    if is_fallback {
        QueueClass::CriticalNow
    } else if terms.visibility_score > 8.0 {
        QueueClass::NextFrame
    } else if terms.camera_velocity_prefetch > 0.5 {
        QueueClass::NearFuture
    } else {
        QueueClass::Speculative
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> EvalContext {
        EvalContext {
            camera: DVec3::new(0.0, 300.0, 0.0),
            motion: DVec3::default(),
            fov_y_radians: 1.0,
            viewport_height: 1080.0,
            base_geometric_error: 4096.0,
        }
    }

    #[test]
    fn closer_tiles_outrank_far() {
        let c = ctx();
        let leaf = TileId::new(8, 10, 10);
        let near = evaluate(&c, 50.0, 0.0, leaf, leaf, false).total();
        let far = evaluate(&c, 8000.0, 0.0, leaf, leaf, false).total();
        assert!(near > far, "near {near} should outrank far {far}");
    }

    #[test]
    fn fallback_is_critical_and_boosted() {
        let c = ctx();
        let leaf = TileId::new(8, 10, 10);
        let anc = TileId::new(2, 0, 0);
        let fb = evaluate(&c, 100.0, 0.0, anc, leaf, true);
        assert!(fb.parent_fallback_risk > 0.0);
        assert_eq!(classify_queue(&fb, true), QueueClass::CriticalNow);
        let non = evaluate(&c, 100.0, 0.0, anc, leaf, false);
        assert!(fb.total() > non.total(), "fallback must boost priority");
    }

    #[test]
    fn velocity_prefetch_favors_motion_direction() {
        let mut c = ctx();
        c.motion = DVec3::new(100.0, 0.0, 0.0); // moving +x
        let leaf = TileId::new(8, 0, 0);
        let ahead = evaluate(&c, 2000.0, 0.0, leaf, leaf, false).camera_velocity_prefetch;
        let behind = evaluate(&c, -2000.0, 0.0, leaf, leaf, false).camera_velocity_prefetch;
        assert!(ahead > behind, "ahead {ahead} should beat behind {behind}");
    }

    #[test]
    fn budgets_from_cache_are_sane() {
        let b = StreamingBudgets::from_cache(64 * 1024 * 1024, 256);
        assert_eq!(b.total_cpu_cache_mb, 64);
        assert_eq!(b.max_inflight_requests, 256);
        assert!(b.max_pages_promoted_per_frame > 0);
    }
}
