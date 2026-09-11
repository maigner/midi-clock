//! CLOCK_MONOTONIC as f64 seconds. This is the single time base shared by the
//! audio thread (capture / playback timestamps), the tracker (beat grid) and
//! the MIDI clock thread.

use libc::{clock_gettime, clock_nanosleep, timespec, CLOCK_MONOTONIC, EINTR, TIMER_ABSTIME};

/// Current monotonic time in seconds.
#[inline]
pub fn now() -> f64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: clock_gettime with a valid pointer never fails for CLOCK_MONOTONIC.
    unsafe {
        clock_gettime(CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as f64 + ts.tv_nsec as f64 * 1e-9
}

/// Sleep until an absolute monotonic time. Returns immediately if `t` is in the past.
pub fn sleep_until(t: f64) {
    if t <= now() {
        return;
    }
    let secs = t.floor();
    let ts = timespec {
        tv_sec: secs as libc::time_t,
        tv_nsec: ((t - secs) * 1e9) as libc::c_long,
    };
    loop {
        // SAFETY: valid timespec, remaining pointer may be null with TIMER_ABSTIME.
        let r = unsafe { clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
        if r != EINTR {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_until_is_accurate_to_a_millisecond() {
        let t = now() + 0.005;
        sleep_until(t);
        let late = now() - t;
        assert!(late >= 0.0, "woke up early by {late}s");
        assert!(late < 0.002, "woke up late by {late}s");
    }
}
