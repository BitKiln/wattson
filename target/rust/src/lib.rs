//! Rust bindings to the target-side C instrumentation library, for testing it on a host.
//!
//! This crate ships nothing. Firmware includes `target/include/wattson.h` and compiles
//! `target/src/wattson.c` directly; what lives here is the harness that lets `cargo test`
//! compile that C, drive it, and feed its output through [`wattson_protocol`]'s decoder — the
//! same decoder the host uses against real hardware.
//!
//! That is the whole idea behind writing the instrumentation library before the profiler
//! board exists: both ends of the wire can be exercised against each other on a laptop, so
//! when silicon does arrive it is the only new variable.
//!
//! # Safety
//!
//! The C library keeps its state in one file-scope struct, so every function here touches
//! shared mutable state and none of it is thread-safe. Tests take [`lock`] for the duration.

use std::sync::{Mutex, MutexGuard};

/// Counters mirroring `pp_stats_t`.
///
/// Every field is a way the timeline can be wrong, which is why the C library exposes them
/// rather than keeping them internal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct Stats {
    /// Events accepted into the ring.
    pub events_recorded: u32,
    /// Events successfully framed and written out.
    pub events_sent: u32,
    /// Events lost — to a full ring, to a missing transport, or to a short write.
    pub events_dropped: u32,
    /// EVENT frames written.
    pub frames_sent: u32,
    /// Encoded bytes written.
    pub bytes_sent: u32,
    /// Short or refused writes from the transport.
    pub write_failures: u32,
}

unsafe extern "C" {
    fn pp_event(id: u16);
    fn pp_event_u32(id: u16, value: u32);
    fn pp_flush() -> usize;
    fn pp_get_stats(out: *mut Stats);
    fn pp_pending() -> u32;
    fn pp_encode_event_frame(
        dst: *mut u8,
        dst_len: usize,
        seq: u8,
        timestamps: *const u32,
        ids: *const u16,
        values: *const u32,
        count: usize,
    ) -> usize;

    // Shim (target/tests/shim.c).
    fn pp_test_reset();
    fn pp_test_reset_without_transport();
    fn pp_test_set_time(t: u32);
    fn pp_test_advance(d: u32);
    fn pp_test_set_write_limit(limit: usize);
    fn pp_test_sink_data() -> *const u8;
    fn pp_test_sink_size() -> usize;
    fn pp_test_macro_event(id: u16);
    fn pp_test_macro_event_u32(id: u16, value: u32);
    fn pp_test_macro_scope(start_id: u16, duration: u32);
    fn pp_test_macro_scope_nested(start_id: u16, step: u32);
    fn pp_test_macro_scope_early_return(start_id: u16, duration: u32) -> i32;
    fn pp_test_macro_scope_explicit(start_id: u16, stop_id: u16, duration: u32);
    fn pp_test_scope_is_safe() -> i32;
    fn pp_test_ring_capacity() -> usize;

    // Built separately with PP_ENABLED=0; referenced so the linker proves that build links.
    fn pp_disabled_smoke(id: u16, value: u32) -> u32;
}

static LOCK: Mutex<()> = Mutex::new(());

/// Serialise access to the C library's single set of statics.
///
/// Poisoning is ignored: a panicking test leaves the C state dirty, but every entry point here
/// begins by resetting it, so the next test starts clean anyway.
pub fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Reset the library, install the capturing transport, and set the clock to zero.
pub fn reset() {
    unsafe { pp_test_reset() }
}

/// Reset with no transport installed, to exercise the unconfigured path.
pub fn reset_without_transport() {
    unsafe { pp_test_reset_without_transport() }
}

/// Set the tick source.
pub fn set_time(t: u32) {
    unsafe { pp_test_set_time(t) }
}

/// Advance the tick source.
pub fn advance(d: u32) {
    unsafe { pp_test_advance(d) }
}

/// Make the transport accept at most `limit` bytes per call. Zero means unlimited.
pub fn set_write_limit(limit: usize) {
    unsafe { pp_test_set_write_limit(limit) }
}

/// Everything the transport has accepted since the last [`reset`].
pub fn sink() -> Vec<u8> {
    unsafe {
        let len = pp_test_sink_size();
        if len == 0 {
            return Vec::new();
        }
        std::slice::from_raw_parts(pp_test_sink_data(), len).to_vec()
    }
}

/// Record a point event through the C function.
pub fn event(id: u16) {
    unsafe { pp_event(id) }
}

/// Record an event carrying a value.
pub fn event_u32(id: u16, value: u32) {
    unsafe { pp_event_u32(id, value) }
}

/// Record through the `PP_EVENT` macro, which is the only way to test the macro itself.
pub fn macro_event(id: u16) {
    unsafe { pp_test_macro_event(id) }
}

/// Record through the `PP_EVENT_U32` macro.
pub fn macro_event_u32(id: u16, value: u32) {
    unsafe { pp_test_macro_event_u32(id, value) }
}

/// Run a `PP_SCOPE` block that takes `duration` ticks.
pub fn macro_scope(start_id: u16, duration: u32) {
    unsafe { pp_test_macro_scope(start_id, duration) }
}

/// Run nested `PP_SCOPE` blocks with the same id, each step taking `step` ticks.
pub fn macro_scope_nested(start_id: u16, step: u32) {
    unsafe { pp_test_macro_scope_nested(start_id, step) }
}

/// Return out of the middle of a `PP_SCOPE` block.
pub fn macro_scope_early_return(start_id: u16, duration: u32) -> i32 {
    unsafe { pp_test_macro_scope_early_return(start_id, duration) }
}

/// Run a `PP_SCOPE_ID` block with an explicit stop id.
pub fn macro_scope_explicit(start_id: u16, stop_id: u16, duration: u32) {
    unsafe { pp_test_macro_scope_explicit(start_id, stop_id, duration) }
}

/// Whether `PP_SCOPE` emits its stop even when the block is left by `return`.
pub fn scope_is_safe() -> bool {
    unsafe { pp_test_scope_is_safe() != 0 }
}

/// The ring capacity the C was compiled with.
pub fn ring_capacity() -> usize {
    unsafe { pp_test_ring_capacity() }
}

/// Encode and write everything queued. Returns encoded bytes written.
pub fn flush() -> usize {
    unsafe { pp_flush() }
}

/// Events currently queued.
pub fn pending() -> u32 {
    unsafe { pp_pending() }
}

/// Snapshot of the counters.
pub fn stats() -> Stats {
    let mut s = Stats::default();
    unsafe { pp_get_stats(&mut s) };
    s
}

/// Encode one EVENT frame from caller-supplied records, bypassing the ring.
///
/// Returns `None` when the arguments do not fit, matching the C function's zero return.
pub fn encode_event_frame(
    seq: u8,
    timestamps: &[u32],
    ids: &[u16],
    values: Option<&[u32]>,
) -> Option<Vec<u8>> {
    assert_eq!(timestamps.len(), ids.len(), "one timestamp per id");
    if let Some(v) = values {
        assert_eq!(v.len(), ids.len(), "one value per id");
    }
    let mut dst = vec![0u8; 1046];
    let n = unsafe {
        pp_encode_event_frame(
            dst.as_mut_ptr(),
            dst.len(),
            seq,
            timestamps.as_ptr(),
            ids.as_ptr(),
            values.map_or(std::ptr::null(), <[u32]>::as_ptr),
            ids.len(),
        )
    };
    if n == 0 {
        return None;
    }
    dst.truncate(n);
    Some(dst)
}

/// Call into the `PP_ENABLED=0` build, proving it linked.
pub fn disabled_smoke(id: u16, value: u32) -> u32 {
    unsafe { pp_disabled_smoke(id, value) }
}
