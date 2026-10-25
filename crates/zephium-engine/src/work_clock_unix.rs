//! The Work clock without a native Work resource port. Same domain as the
//! macOS and Windows port: one process origin, set at the first Work edge.

use std::sync::OnceLock;
use std::time::Instant;

use zephium_agentic::AgentPolicyInstant;

static WORK_CLOCK_ORIGIN: OnceLock<Instant> = OnceLock::new();

pub fn work_browser_monotonic_now() -> Option<AgentPolicyInstant> {
    u64::try_from(
        WORK_CLOCK_ORIGIN
            .get_or_init(Instant::now)
            .elapsed()
            .as_millis(),
    )
    .ok()
    .map(AgentPolicyInstant::from_millis)
}

pub fn work_browser_monotonic_deadline(deadline: Instant) -> Option<AgentPolicyInstant> {
    let origin = *WORK_CLOCK_ORIGIN.get_or_init(Instant::now);
    let nanos = deadline.checked_duration_since(origin)?.as_nanos();
    u64::try_from(nanos.div_ceil(1_000_000))
        .ok()
        .map(AgentPolicyInstant::from_millis)
}
