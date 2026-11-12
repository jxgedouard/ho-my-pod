//! The sender clock. Receivers sync to it through NTP-style timing requests,
//! and every sync packet we send is expressed in it. It must be monotonic, so
//! it is an `Instant` anchored once to wall-clock time.

use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Seconds between 1900-01-01 (NTP epoch) and 1970-01-01 (Unix epoch).
const NTP_EPOCH_DELTA: u64 = 2_208_988_800;

struct Base {
    instant: Instant,
    unix_nanos: u128,
}

fn base() -> &'static Base {
    static BASE: OnceLock<Base> = OnceLock::new();
    BASE.get_or_init(|| Base {
        instant: Instant::now(),
        unix_nanos: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    })
}

/// NTP 32.32 fixed-point timestamp for a given instant.
pub fn ntp_at(t: Instant) -> u64 {
    let b = base();
    let total = match t.checked_duration_since(b.instant) {
        Some(after) => b.unix_nanos + after.as_nanos(),
        None => b.unix_nanos - (b.instant - t).as_nanos(),
    };
    let secs = (total / 1_000_000_000) as u64 + NTP_EPOCH_DELTA;
    let nanos = (total % 1_000_000_000) as u64;
    let frac = (nanos << 32) / 1_000_000_000;
    (secs << 32) | frac
}

pub fn ntp_now() -> u64 {
    ntp_at(Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn half_second_is_half_of_frac_range() {
        let t0 = Instant::now();
        let a = ntp_at(t0);
        let b = ntp_at(t0 + Duration::from_millis(500));
        let diff = b - a;
        let expected = 1u64 << 31;
        assert!((diff as i64 - expected as i64).abs() < 10, "diff={diff}");
    }
}
