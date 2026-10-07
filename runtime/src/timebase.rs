//! The guest's timebase. The generated code implements `mftb` with the host's cycle counter
//! (XenonRecomp's `__rdtsc`), so the performance frequency the title asks for must be that
//! counter's rate, not the console's 50 MHz.

use std::sync::OnceLock;

/// The counter `mftb` reads in the generated code.
#[cfg(test)]
fn counter() -> u64 {
    platform::counter()
}

/// Ticks per second of `counter`, measured or read once and then cached.
pub fn frequency() -> u64 {
    static FREQUENCY: OnceLock<u64> = OnceLock::new();
    *FREQUENCY.get_or_init(platform::frequency)
}

#[cfg(target_arch = "x86_64")]
mod platform {
    use std::thread;
    use std::time::{Duration, Instant};

    /// Long enough that sleep and clock jitter stay well under 0.1% of the measured rate.
    const CALIBRATION_PERIOD: Duration = Duration::from_millis(50);
    const NANOSECONDS_PER_SECOND: u128 = 1_000_000_000;

    pub fn counter() -> u64 {
        // SAFETY: rdtsc has no preconditions on x86-64.
        unsafe { core::arch::x86_64::_rdtsc() }
    }

    /// x86 has no portable way to read the TSC rate, so measure it against the OS clock.
    pub fn frequency() -> u64 {
        let (start_time, start_ticks) = (Instant::now(), counter());
        thread::sleep(CALIBRATION_PERIOD);
        let (elapsed, ticks) = (start_time.elapsed(), counter() - start_ticks);
        (u128::from(ticks) * NANOSECONDS_PER_SECOND / elapsed.as_nanos()) as u64
    }
}

#[cfg(target_arch = "aarch64")]
mod platform {
    #[cfg(test)]
    pub fn counter() -> u64 {
        let ticks: u64;
        // SAFETY: the virtual counter is readable from user space on every supported OS.
        unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) ticks) };
        ticks
    }

    pub fn frequency() -> u64 {
        let frequency: u64;
        // SAFETY: as above, for the counter's frequency register.
        unsafe { core::arch::asm!("mrs {}, cntfrq_el0", out(reg) frequency) };
        frequency
    }
}

#[cfg(test)]
mod tests {
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn counter_advances_at_the_reported_frequency() {
        const SAMPLE: Duration = Duration::from_millis(200);
        /// Generous, so a busy CI machine does not make the test flaky.
        const TOLERANCE: f64 = 0.05;
        let frequency = frequency() as f64;
        let (start_time, start_ticks) = (Instant::now(), counter());
        thread::sleep(SAMPLE);
        let measured = (counter() - start_ticks) as f64 / start_time.elapsed().as_secs_f64();
        assert!(
            (measured / frequency - 1.0).abs() < TOLERANCE,
            "reported {frequency} Hz, measured {measured} Hz"
        );
    }
}
