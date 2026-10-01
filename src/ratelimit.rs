// Throttles guessing attempts against a secret (browser login, mesh handshake).
//
// Every attempt is counted when it starts and refunded only if it succeeds, so
// a burst of parallel connections can't all slip through before the first
// failure is recorded. After `free_attempts` unrefunded attempts an IP is
// locked out for an exponentially growing time.

use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// How long an IP's failure count is remembered once it stops trying.
const FORGET_AFTER: Duration = Duration::from_secs(3600);
// Bound on tracked IPs so a client rotating addresses can't grow the map forever.
const MAX_TRACKED_IPS: usize = 4096;

pub struct Policy {
    pub free_attempts: u32,
    pub base_lockout: Duration,
    pub max_lockout: Duration,
    // (max failures, window) across all IPs; stops an attacker rotating addresses.
    pub global_cap: Option<(usize, Duration)>,
}

struct Entry {
    unrefunded: u32,
    locked_until: Option<Instant>,
    last_attempt: Instant,
}

#[derive(Default)]
struct Inner {
    by_ip: HashMap<IpAddr, Entry>,
    recent: VecDeque<Instant>,
}

// Proof that `begin` allowed an attempt; handing it back to `succeed` removes
// exactly that attempt from the global count.
#[derive(Debug)]
pub struct Ticket(Instant);

pub struct AttemptLimiter {
    policy: Policy,
    inner: Mutex<Inner>,
}

impl AttemptLimiter {
    pub fn new(policy: Policy) -> Self {
        Self { policy, inner: Mutex::new(Inner::default()) }
    }

    // Registers an attempt. Err(retry_after) means the caller must reject it
    // without checking the secret.
    pub fn begin(&self, ip: IpAddr) -> Result<Ticket, Duration> {
        let now = Instant::now();
        let mut inner = self.inner.lock().unwrap();

        if let Some((max, window)) = self.policy.global_cap {
            while inner.recent.front().is_some_and(|t| now.duration_since(*t) >= window) {
                inner.recent.pop_front();
            }
            if inner.recent.len() >= max {
                return Err(window.saturating_sub(now.duration_since(inner.recent[0])));
            }
        }

        if inner.by_ip.len() >= MAX_TRACKED_IPS {
            inner.by_ip.retain(|_, e| e.locked_until.is_some_and(|t| t > now));
        }

        let entry = inner.by_ip.entry(ip).or_insert(Entry { unrefunded: 0, locked_until: None, last_attempt: now });
        if now.duration_since(entry.last_attempt) > FORGET_AFTER {
            entry.unrefunded = 0;
            entry.locked_until = None;
        }
        if let Some(until) = entry.locked_until {
            if until > now {
                return Err(until - now);
            }
        }

        entry.last_attempt = now;
        entry.unrefunded += 1;
        if entry.unrefunded > self.policy.free_attempts {
            let doublings = (entry.unrefunded - self.policy.free_attempts - 1).min(20);
            let lockout = self.policy.base_lockout.saturating_mul(1 << doublings).min(self.policy.max_lockout);
            entry.locked_until = Some(now + lockout);
        }
        if self.policy.global_cap.is_some() {
            inner.recent.push_back(now);
        }
        Ok(Ticket(now))
    }

    // Only for an attempt that proved knowledge of the secret: it clears the
    // IP's record and un-counts the attempt globally. Rejections that aren't a
    // wrong guess (e.g. a duplicate connection) must not call this, or a
    // guesser could use them to reset their own lockout.
    pub fn succeed(&self, ip: IpAddr, ticket: Ticket) {
        let mut inner = self.inner.lock().unwrap();
        inner.by_ip.remove(&ip);
        if let Some(pos) = inner.recent.iter().position(|t| *t == ticket.0) {
            inner.recent.remove(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 10));
    const OTHER_IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 11));

    fn policy(global_cap: Option<(usize, Duration)>) -> Policy {
        Policy {
            free_attempts: 3,
            base_lockout: Duration::from_secs(10),
            max_lockout: Duration::from_secs(80),
            global_cap,
        }
    }

    #[test]
    fn locks_out_after_free_attempts() {
        let limiter = AttemptLimiter::new(policy(None));
        for _ in 0..3 {
            assert!(limiter.begin(IP).is_ok());
        }
        // The 4th attempt is still allowed but starts the lockout.
        assert!(limiter.begin(IP).is_ok());
        let retry = limiter.begin(IP).unwrap_err();
        assert!(retry > Duration::from_secs(9) && retry <= Duration::from_secs(10));
    }

    #[test]
    fn lockout_is_per_ip() {
        let limiter = AttemptLimiter::new(policy(None));
        for _ in 0..4 {
            let _ = limiter.begin(IP);
        }
        assert!(limiter.begin(IP).is_err());
        assert!(limiter.begin(OTHER_IP).is_ok());
    }

    #[test]
    fn success_clears_the_count() {
        let limiter = AttemptLimiter::new(policy(None));
        for _ in 0..10 {
            let ticket = limiter.begin(IP).unwrap();
            limiter.succeed(IP, ticket);
        }
    }

    #[test]
    fn success_does_not_count_towards_the_global_cap() {
        let limiter = AttemptLimiter::new(policy(Some((2, Duration::from_secs(600)))));
        for _ in 0..10 {
            let ticket = limiter.begin(IP).unwrap();
            limiter.succeed(IP, ticket);
        }
        assert!(limiter.begin(IP).is_ok());
    }

    #[test]
    fn lockout_grows_exponentially_and_is_capped() {
        let limiter = AttemptLimiter::new(policy(None));
        let mut lockouts = Vec::new();
        for _ in 0..8 {
            // Force each attempt through by clearing the lock, keeping the count.
            if let Some(entry) = limiter.inner.lock().unwrap().by_ip.get_mut(&IP) {
                entry.locked_until = None;
            }
            let _ = limiter.begin(IP);
            let until = limiter.inner.lock().unwrap().by_ip[&IP].locked_until;
            lockouts.push(until.map(|u| u.duration_since(Instant::now()).as_secs()));
        }
        assert_eq!(lockouts[..3], [None, None, None]);
        assert!(lockouts[3].unwrap() <= 10);
        assert!(lockouts[4].unwrap() > 10 && lockouts[4].unwrap() <= 20);
        assert!(lockouts[5].unwrap() > 20 && lockouts[5].unwrap() <= 40);
        assert!(lockouts[7].unwrap() > 40 && lockouts[7].unwrap() <= 80);
    }

    #[test]
    fn global_cap_blocks_every_ip() {
        let limiter = AttemptLimiter::new(policy(Some((5, Duration::from_secs(600)))));
        for i in 0..5u8 {
            let ip = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, i));
            assert!(limiter.begin(ip).is_ok());
        }
        assert!(limiter.begin(OTHER_IP).is_err());
    }

    #[test]
    fn tracked_ips_stay_bounded() {
        let limiter = AttemptLimiter::new(policy(None));
        for i in 0..(MAX_TRACKED_IPS as u32 + 100) {
            let _ = limiter.begin(IpAddr::V4(std::net::Ipv4Addr::from(i)));
        }
        assert!(limiter.inner.lock().unwrap().by_ip.len() <= MAX_TRACKED_IPS + 1);
    }
}
