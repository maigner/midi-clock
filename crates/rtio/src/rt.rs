//! Real-time thread helpers. All of these are best-effort: on a machine without
//! rtprio limits they fail with a readable error and the caller keeps going.

use std::io;

/// Put the calling thread on SCHED_FIFO with the given priority (1..=99).
pub fn set_realtime(priority: i32) -> Result<(), String> {
    let param = libc::sched_param { sched_priority: priority };
    // SAFETY: pid 0 means the calling thread; param is valid.
    let r = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if r == 0 {
        Ok(())
    } else {
        Err(format!("sched_setscheduler(SCHED_FIFO, {priority}): {}", io::Error::last_os_error()))
    }
}

/// Pin the calling thread to one CPU.
pub fn pin_to_cpu(cpu: usize) -> Result<(), String> {
    // SAFETY: cpu_set_t is a plain bitmask struct; zeroed is a valid empty set.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(cpu, &mut set);
        let r = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        if r == 0 {
            Ok(())
        } else {
            Err(format!("sched_setaffinity(cpu {cpu}): {}", io::Error::last_os_error()))
        }
    }
}

/// Lock all current and future pages of the process into RAM.
pub fn lock_memory() -> Result<(), String> {
    // SAFETY: plain syscall with constant flags.
    let r = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if r == 0 {
        Ok(())
    } else {
        Err(format!("mlockall: {}", io::Error::last_os_error()))
    }
}

/// Apply a thread's real-time setup and report what could not be applied.
/// Never fails: real-time is a quality-of-service matter, not a correctness one.
pub fn setup_thread(name: &str, priority: i32, cpu: Option<usize>) -> Vec<String> {
    let mut warnings = Vec::new();
    if priority > 0 {
        if let Err(e) = set_realtime(priority) {
            warnings.push(format!("{name}: {e}"));
        }
    }
    if let Some(c) = cpu {
        if let Err(e) = pin_to_cpu(c) {
            warnings.push(format!("{name}: {e}"));
        }
    }
    warnings
}
