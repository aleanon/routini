//! Per-client request rate limiting (nginx `limit_req` with `burst`, and `nodelay` or not).
//!
//! A generic cell rate algorithm (GCRA), the leaky bucket nginx uses: each client has a
//! theoretical arrival time (TAT), advanced by one emission interval (`1 / rps`) per request.
//! A request arriving more than `burst` intervals before its TAT is over the limit. Otherwise it
//! may pass at once, or, in delay mode, wait until its slot so requests leave at the configured
//! rate. A TAT in the past is the same as no entry at all, so stale entries can be dropped at any
//! time: memory follows the clients active within the last burst window.
//!
//! That memory is bounded too. IPv6 clients are counted per /64 (one subscriber; a single host
//! can use any address in its /64). Each shard holds at most [MAX_PER_SHARD] clients; when it
//! is full of live entries an arbitrary one is evicted to make room. Under such a flood a client
//! may get its burst back sooner, but nobody passes untracked and no newcomer is locked out. A
//! full shard purges at most once a second, so a flood of new clients can't make every request
//! scan it.

use std::{
    collections::HashMap,
    hash::{BuildHasher, RandomState},
    net::{IpAddr, Ipv6Addr},
    sync::{
        LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

const SHARDS: usize = 32;
/// A shard's size at which stale entries are dropped for the first time.
const MIN_PURGE_AT: usize = 1024;
/// Clients tracked per shard, about 13 MiB in all.
const MAX_PER_SHARD: usize = 8192;
/// How often a shard that is full of live entries may be scanned for expired ones.
const FULL_PURGE_INTERVAL: Duration = Duration::from_secs(1);

/// Rate limit settings of a route.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimit {
    /// Sustained requests per second per client.
    pub rps: f64,
    /// Requests a client may send above the rate at once (nginx `burst`).
    pub burst: u32,
    /// Hold requests within the burst until their slot instead of passing them at once (nginx
    /// without `nodelay`).
    pub delay: bool,
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    Allow,
    /// Allow after waiting this long.
    Delay(Duration),
    Reject,
}

struct Shard {
    tats: HashMap<IpAddr, Instant>,
    /// Size at which the next purge of stale entries runs. Doubles with the live entries, so
    /// purging stays amortised O(1) per request.
    purge_at: usize,
    last_purge: Option<Instant>,
}

impl Shard {
    /// Make room for a new client: drop expired entries if a purge is due, and evict an
    /// arbitrary client if the shard is still full.
    fn make_room(&mut self, now: Instant) {
        let full = self.tats.len() >= MAX_PER_SHARD;
        let due = self.tats.len() >= self.purge_at
            && (!full
                || self
                    .last_purge
                    .is_none_or(|t| now.saturating_duration_since(t) >= FULL_PURGE_INTERVAL));
        if due {
            self.tats.retain(|_, tat| *tat > now);
            self.purge_at = (self.tats.len() * 2).clamp(MIN_PURGE_AT, MAX_PER_SHARD);
            self.last_purge = Some(now);
        }
        if self.tats.len() >= MAX_PER_SHARD {
            if let Some(evict) = self.tats.keys().next().copied() {
                self.tats.remove(&evict);
            }
            warn_full(now);
        }
    }
}

/// The key a client is counted under: its address, or its /64 for IPv6.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from_bits(v6.to_bits() & !u128::from(u64::MAX))),
        v4 => v4,
    }
}

/// Warn that limiters are evicting clients, at most once a minute.
fn warn_full(now: Instant) {
    static START: LazyLock<Instant> = LazyLock::new(Instant::now);
    static LAST_WARNED: AtomicU64 = AtomicU64::new(0);
    // Seconds since START, offset by one so 0 means never
    let secs = now.saturating_duration_since(*START).as_secs() + 1;
    let last = LAST_WARNED.load(Ordering::Relaxed);
    if (last == 0 || secs >= last + 60)
        && LAST_WARNED
            .compare_exchange(last, secs, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            "rate limiter is tracking its maximum number of clients and evicting some, so they \
             may get their burst back early"
        );
    }
}

pub struct RateLimiter {
    interval: Duration,
    /// How far ahead of now a client's TAT may be (`burst` intervals).
    tolerance: Duration,
    delay: bool,
    hasher: RandomState,
    shards: Box<[Mutex<Shard>]>,
}

impl RateLimiter {
    /// `None` if `rps` isn't a usable rate (not positive, or so small the interval overflows).
    pub fn new(limit: RateLimit) -> Option<Self> {
        let interval = Duration::try_from_secs_f64(1.0 / limit.rps).ok()?;
        // Keep TATs far from Instant's range
        if interval.is_zero() || interval > Duration::from_secs(1 << 32) {
            return None;
        }
        Some(Self {
            interval,
            tolerance: interval.checked_mul(limit.burst)?,
            delay: limit.delay,
            hasher: RandomState::new(),
            shards: (0..SHARDS)
                .map(|_| {
                    Mutex::new(Shard {
                        tats: HashMap::new(),
                        purge_at: MIN_PURGE_AT,
                        last_purge: None,
                    })
                })
                .collect(),
        })
    }

    /// Account for a request from `client` at `now`.
    pub fn check(&self, client: IpAddr, now: Instant) -> Decision {
        let client = client_key(client);
        let shard = &self.shards[self.hasher.hash_one(client) as usize % SHARDS];
        let mut shard = shard
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let tat = match shard.tats.get(&client).copied() {
            Some(tat) => tat.max(now),
            None => {
                shard.make_room(now);
                now
            }
        };
        let ahead = tat - now;
        if ahead > self.tolerance {
            return Decision::Reject;
        }
        shard.tats.insert(client, tat + self.interval);

        if self.delay && !ahead.is_zero() {
            Decision::Delay(ahead)
        } else {
            Decision::Allow
        }
    }

    #[cfg(test)]
    fn entries(&self) -> usize {
        self.shards
            .iter()
            .map(|s| s.lock().unwrap().tats.len())
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 1));
    const B: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(198, 51, 100, 2));

    fn limiter(rps: f64, burst: u32, delay: bool) -> RateLimiter {
        RateLimiter::new(RateLimit { rps, burst, delay }).unwrap()
    }

    #[test]
    fn unusable_rates_are_refused() {
        for rps in [0.0, -1.0, f64::NAN, f64::MIN_POSITIVE, f64::INFINITY] {
            let limit = RateLimit {
                rps,
                burst: 0,
                delay: false,
            };
            assert!(RateLimiter::new(limit).is_none(), "{rps}");
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn without_burst_requests_must_be_spaced_at_the_rate() {
        let l = limiter(10.0, 0, false);
        let t = Instant::now();
        assert_eq!(l.check(A, t), Decision::Allow);
        assert_eq!(l.check(A, t + ms(50)), Decision::Reject);
        assert_eq!(l.check(A, t + ms(100)), Decision::Allow);
        // Clients are counted separately
        assert_eq!(l.check(B, t + ms(100)), Decision::Allow);
    }

    #[test]
    fn burst_passes_at_once_then_the_rate_applies() {
        let l = limiter(10.0, 5, false);
        let t = Instant::now();
        for i in 0..6 {
            assert_eq!(l.check(A, t), Decision::Allow, "request {i}");
        }
        assert_eq!(l.check(A, t), Decision::Reject);
        // One slot frees up per interval
        assert_eq!(l.check(A, t + ms(100)), Decision::Allow);
        assert_eq!(l.check(A, t + ms(100)), Decision::Reject);
        // Rejected requests don't use up slots: idle a full burst and it's back
        for i in 0..6 {
            assert_eq!(l.check(A, t + ms(700)), Decision::Allow, "request {i}");
        }
    }

    #[test]
    fn delay_mode_spaces_burst_requests_at_the_rate() {
        let l = limiter(10.0, 3, true);
        let t = Instant::now();
        assert_eq!(l.check(A, t), Decision::Allow);
        assert_eq!(l.check(A, t), Decision::Delay(ms(100)));
        assert_eq!(l.check(A, t), Decision::Delay(ms(200)));
        assert_eq!(l.check(A, t), Decision::Delay(ms(300)));
        assert_eq!(l.check(A, t), Decision::Reject);
    }

    #[test]
    fn ipv6_clients_are_counted_per_64() {
        let l = limiter(10.0, 0, false);
        let t = Instant::now();
        assert_eq!(
            l.check("2001:db8:1:2::1".parse().unwrap(), t),
            Decision::Allow
        );
        assert_eq!(
            l.check("2001:db8:1:2:ffff::9".parse().unwrap(), t),
            Decision::Reject,
            "same /64"
        );
        assert_eq!(
            l.check("2001:db8:1:3::1".parse().unwrap(), t),
            Decision::Allow
        );
    }

    #[test]
    fn a_full_limiter_evicts_to_keep_limiting_new_clients() {
        // Long-lived entries: 1 request per 100s
        let l = limiter(0.01, 0, false);
        let t = Instant::now();
        let mut i = 0u32;
        while l.entries() < SHARDS * MAX_PER_SHARD {
            l.check(IpAddr::V4(i.into()), t);
            i += 1;
        }
        // New clients are tracked and limited, the memory stays capped
        for new in (u32::MAX - 100..=u32::MAX).map(|i| IpAddr::V4(i.into())) {
            assert_eq!(l.check(new, t), Decision::Allow);
            assert_eq!(l.check(new, t), Decision::Reject);
        }
        assert_eq!(l.entries(), SHARDS * MAX_PER_SHARD);
    }

    #[test]
    fn stale_entries_are_dropped() {
        let l = limiter(1000.0, 0, false);
        let t = Instant::now();
        // ~2048 per shard, all expiring 1ms later. No purge can drop them yet
        let stale = SHARDS * MIN_PURGE_AT * 2;
        for i in 0..stale as u32 {
            l.check(IpAddr::V4(i.into()), t);
        }
        assert_eq!(l.entries(), stale);
        // Each shard reaches its purge size (4096) again, dropping what expired
        let fresh = SHARDS * MIN_PURGE_AT * 4;
        let later = t + ms(10);
        for i in 0..fresh as u32 {
            l.check(IpAddr::V4((u32::MAX - i).into()), later);
        }
        assert!(l.entries() < fresh + stale / 4, "{}", l.entries());
    }
}
