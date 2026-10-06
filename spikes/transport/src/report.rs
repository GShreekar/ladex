use std::time::Duration;

use serde::Serialize;

/// One JSON line per process run, collected by bench.sh.
#[derive(Debug, Serialize)]
pub struct Report {
    pub transport: &'static str,
    pub role: &'static str,
    pub bytes: u64,
    /// Hashing/importing on the sender; zero on the receiver.
    pub prepare_secs: f64,
    pub connect_secs: f64,
    /// First request until every byte is verified and written.
    pub transfer_secs: f64,
    /// fsync, plus the export out of the blob store for iroh.
    pub finalize_secs: f64,
}

impl Report {
    pub fn print(&self) {
        let mut line = serde_json::to_value(self).expect("a report always serializes");
        line["transfer_mib_per_sec"] = mib_per_sec(self.bytes, self.transfer_secs).into();
        let usage = ResourceUsage::of_this_process();
        line["cpu_user_secs"] = usage.user_secs.into();
        line["cpu_system_secs"] = usage.system_secs.into();
        line["max_rss_mib"] = usage.max_rss_mib.into();
        println!("{line}");
    }
}

pub fn secs(duration: Duration) -> f64 {
    duration.as_secs_f64()
}

fn mib_per_sec(bytes: u64, secs: f64) -> f64 {
    if secs == 0.0 {
        return 0.0;
    }
    bytes as f64 / (1u64 << 20) as f64 / secs
}

/// CPU time and peak memory of the whole process, read once the transfer is over.
struct ResourceUsage {
    user_secs: f64,
    system_secs: f64,
    max_rss_mib: f64,
}

impl ResourceUsage {
    #[cfg(unix)]
    fn of_this_process() -> Self {
        // SAFETY: getrusage only writes into the zeroed struct we pass it.
        let usage = unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            libc::getrusage(libc::RUSAGE_SELF, &mut usage);
            usage
        };
        let to_secs = |time: libc::timeval| time.tv_sec as f64 + time.tv_usec as f64 / 1e6;
        // Linux reports ru_maxrss in KiB.
        Self { user_secs: to_secs(usage.ru_utime), system_secs: to_secs(usage.ru_stime), max_rss_mib: usage.ru_maxrss as f64 / 1024.0 }
    }

    #[cfg(not(unix))]
    fn of_this_process() -> Self {
        Self { user_secs: 0.0, system_secs: 0.0, max_rss_mib: 0.0 }
    }
}
