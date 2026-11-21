//! Wall-clock helper; timestamps are Unix milliseconds throughout brumm.
//! `n0_future::time::SystemTime` is `std`'s natively and `Date.now()` in the browser.

use n0_future::time::SystemTime;

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// The clock nostr reads through `universal-time` in wasm. Each wasm binary registers it once
/// with [`crate::register_browser_clock!`]; the linker drops it from a library that nothing calls.
#[cfg(target_family = "wasm")]
pub struct BrowserClock;

#[cfg(target_family = "wasm")]
static ORIGIN: std::sync::OnceLock<n0_future::time::Instant> = std::sync::OnceLock::new();

#[cfg(target_family = "wasm")]
impl universal_time::WallClock for BrowserClock {
    fn system_time(&self) -> universal_time::SystemTime {
        let since_epoch = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default();
        universal_time::SystemTime::from_unix_duration(since_epoch)
    }
}

#[cfg(target_family = "wasm")]
impl universal_time::MonotonicClock for BrowserClock {
    fn instant(&self) -> universal_time::Instant {
        let origin = ORIGIN.get_or_init(n0_future::time::Instant::now);
        universal_time::Instant::from_ticks(origin.elapsed())
    }
}

/// Registers [`BrowserClock`] as the `universal-time` provider. Invoke once in each wasm binary.
#[cfg(target_family = "wasm")]
#[macro_export]
macro_rules! register_browser_clock {
    () => {
        $crate::__universal_time::define_time_provider!($crate::clock::BrowserClock);
    };
}
