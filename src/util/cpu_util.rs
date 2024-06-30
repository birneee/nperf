use libc::{getrusage, rusage, RUSAGE_SELF};
use std::time::{Instant, SystemTime};

pub struct CpuUtil {
    last_instant: Instant,
    last_clock: f64,
    last_usage: rusage,
    first_usage: rusage
}

impl CpuUtil {
    pub fn new() -> Self {
        let mut last_usage = unsafe { std::mem::zeroed() };
        unsafe {
            getrusage(RUSAGE_SELF, &mut last_usage);
        }
        let last_clock = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to get system time")
            .as_secs_f64();
        CpuUtil {
            last_instant: Instant::now(),
            last_clock,
            last_usage,
            first_usage: last_usage
        }
    }

    // Very similar code to iperf3, but with some modifications to rustify it
    fn get_cpu_util(&mut self, absolut_cpu_util: bool) -> (f64, f64) {
        let now = Instant::now();
        let current_clock = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("Failed to get system time")
            .as_secs_f64();
        let mut current_usage = unsafe { std::mem::zeroed() };
        unsafe {
            getrusage(RUSAGE_SELF, &mut current_usage);
        }

        let rusage = if absolut_cpu_util {
            self.first_usage
        } else {
            self.last_usage
        };

        let timediff = now.duration_since(self.last_instant).as_micros() as f64;
        let userdiff = (current_usage.ru_utime.tv_sec as f64 * 1_000_000.0 + current_usage.ru_utime.tv_usec as f64)
            - (rusage.ru_utime.tv_sec as f64 * 1_000_000.0 + rusage.ru_utime.tv_usec as f64);
        let systemdiff = (current_usage.ru_stime.tv_sec as f64 * 1_000_000.0 + current_usage.ru_stime.tv_usec as f64)
            - (rusage.ru_stime.tv_sec as f64 * 1_000_000.0 + rusage.ru_stime.tv_usec as f64);

        // Update last measurements
        self.last_instant = now;
        self.last_clock = current_clock;
        self.last_usage = current_usage;

        // userspace, system
        ((userdiff / timediff) * 100.0, (systemdiff / timediff) * 100.0)
    }

    pub fn get_relative_cpu_util(&mut self) -> (f64, f64) {
        self.get_cpu_util(false)
    }

    pub fn get_absolut_cpu_util(&mut self) -> (f64, f64) {
        self.get_cpu_util(true)
    }
}