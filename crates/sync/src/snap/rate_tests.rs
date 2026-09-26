//! What a peer may pull, and what tracking it costs us.

use super::rate::{Allowance, RateLimiter};
use alloy_primitives::B512;
use std::time::Duration;

fn limiter(per_peer: u64, global: u64) -> RateLimiter {
    RateLimiter::new(per_peer, global, Duration::from_secs(5), 1024)
}

/// A peer within its rate is served without delay, burst included: an honest
/// client's parallel requests should go out together, not be spread into a
/// stutter.
#[test]
fn a_peer_within_its_rate_waits_for_nothing() {
    let rate = limiter(8 * 1024 * 1024, 32 * 1024 * 1024);
    let peer = B512::repeat_byte(1);

    // Eight cells of 100 KB is well inside a 2-second burst of 8 MB/s.
    for i in 0..8 {
        assert_eq!(rate.allow(peer, 100_000), Allowance::Now, "cell {i} was delayed");
    }
}

/// **The limit that matters.** A peer pulling faster than its rate is slowed
/// down, not told no -- backpressure needs nothing of the protocol.
#[test]
fn a_peer_over_its_rate_is_made_to_wait() {
    let rate = limiter(1024 * 1024, 32 * 1024 * 1024);
    let peer = B512::repeat_byte(1);

    // Drain the burst.
    let mut delayed = 0;
    for _ in 0..40 {
        match rate.allow(peer, 100_000) {
            Allowance::Now => {}
            Allowance::After(d) => {
                assert!(d > Duration::ZERO);
                delayed += 1;
            }
            Allowance::Refuse => break,
        }
    }
    assert!(delayed > 0, "a peer well over its rate was never slowed");
}

/// Far enough over and it is refused instead, so it goes elsewhere rather
/// than holding one of its three slots open on us.
#[test]
fn a_peer_far_over_its_rate_is_refused() {
    let rate = limiter(100_000, 32 * 1024 * 1024);
    let peer = B512::repeat_byte(1);

    let mut refused = false;
    for _ in 0..100 {
        if rate.allow(peer, 100_000) == Allowance::Refuse {
            refused = true;
            break;
        }
    }
    assert!(refused, "a peer pulling a hundred times its rate was never refused");
}

/// One well-behaved peer cannot be starved by another's greed: the limits are
/// per peer, so a greedy one spends only its own allowance.
#[test]
fn one_peer_cannot_spend_anothers_allowance() {
    let rate = limiter(1024 * 1024, 64 * 1024 * 1024);
    let greedy = B512::repeat_byte(1);
    let honest = B512::repeat_byte(2);

    for _ in 0..200 {
        let _ = rate.allow(greedy, 100_000);
    }

    assert_eq!(
        rate.allow(honest, 100_000),
        Allowance::Now,
        "a quiet peer was charged for a greedy one"
    );
}

/// But the whole-server limit still binds, so many peers at their individual
/// rates cannot together saturate the uplink.
#[test]
fn the_whole_server_limit_binds_across_peers() {
    // Each peer is generous, the server is not.
    let rate = limiter(8 * 1024 * 1024, 1024 * 1024);

    let mut throttled = false;
    for p in 0..40u8 {
        let peer = B512::repeat_byte(p);
        for _ in 0..4 {
            if rate.allow(peer, 100_000) != Allowance::Now {
                throttled = true;
            }
        }
    }
    assert!(throttled, "forty peers together never hit the server limit");
}

/// **The memory question.** Tracking is per peer, not per peer and cell, and
/// the map is bounded however many distinct peers appear.
///
/// This is why repeats are not remembered: doing so would be O(peers × cells)
/// -- 9,200 cells against every peer on mainnet -- and a peer could inflate it
/// deliberately. Turning a bandwidth problem into a memory problem is a bad
/// trade.
#[test]
fn tracking_is_bounded_however_many_peers_appear() {
    let rate = RateLimiter::new(8 * 1024 * 1024, 32 * 1024 * 1024, Duration::from_secs(5), 64);

    // Far more distinct peers than the bound, each asking once.
    for i in 0..5_000u32 {
        let mut bytes = [0u8; 64];
        bytes[..4].copy_from_slice(&i.to_be_bytes());
        rate.allow(B512::from(bytes), 1_000);
    }

    let tracked = rate.tracked();
    assert!(
        tracked <= 128,
        "5000 peers left {tracked} buckets behind; the map is not bounded"
    );
}

/// Asking for the same cell over and over costs a peer exactly what asking
/// for different ones costs. Nothing needs to know they were the same.
#[test]
fn a_repeated_request_costs_the_same_as_a_fresh_one() {
    let same = limiter(1024 * 1024, 32 * 1024 * 1024);
    let varied = limiter(1024 * 1024, 32 * 1024 * 1024);
    let peer = B512::repeat_byte(7);

    let mut same_allowed = 0;
    let mut varied_allowed = 0;
    for _ in 0..30 {
        if same.allow(peer, 100_000) == Allowance::Now {
            same_allowed += 1;
        }
        if varied.allow(peer, 100_000) == Allowance::Now {
            varied_allowed += 1;
        }
    }
    assert_eq!(
        same_allowed, varied_allowed,
        "repeats were charged differently from fresh requests"
    );
}

/// An idle peer's allowance comes back, and how long that takes is bounded by
/// `max_wait` -- a peer may run that far ahead of its rate and no further, so
/// the worst it can do to itself is that much of a pause.
#[test]
fn allowance_returns_within_the_bounded_debt() {
    let max_wait = Duration::from_millis(200);
    let rate = RateLimiter::new(10_000_000, 32 * 1024 * 1024, max_wait, 1024);
    let peer = B512::repeat_byte(3);

    // Spend until it will not serve at all.
    let mut refusals = 0;
    for _ in 0..500 {
        if rate.allow(peer, 1_000_000) == Allowance::Refuse {
            refusals += 1;
            if refusals > 3 {
                break;
            }
        }
    }
    assert!(refusals > 0, "a peer fifty times over its rate was never refused");

    // Debt is bounded by rate * max_wait, so a pause of that order clears it.
    // A little slack: the point is that the lockout is bounded, not exact.
    std::thread::sleep(max_wait * 3);
    assert_eq!(
        rate.allow(peer, 1_000_000),
        Allowance::Now,
        "the debt outlived the bound it is supposed to have"
    );
}

/// A refused request is not charged. Otherwise a peer would dig itself deeper
/// for an answer it never got, and an honest client arriving while the
/// whole-server limit was binding would be punished for someone else's greed.
#[test]
fn a_refused_request_costs_nothing() {
    // 1 MB/s, 100 KB requests, refuse past a 300 ms wait. Debt is therefore
    // bounded at 100_000 - 300_000 = -200_000 bytes, which clears in 300 ms.
    let rate = RateLimiter::new(
        1_000_000,
        32 * 1024 * 1024,
        Duration::from_millis(300),
        1024,
    );
    let peer = B512::repeat_byte(9);

    while rate.allow(peer, 100_000) != Allowance::Refuse {}

    // Keep asking while refused. If refusals were charged, each would deepen
    // the debt and the peer would never climb out.
    for _ in 0..500 {
        let _ = rate.allow(peer, 100_000);
    }

    std::thread::sleep(Duration::from_millis(900));
    assert_eq!(
        rate.allow(peer, 100_000),
        Allowance::Now,
        "500 refusals left a debt, so they were charged"
    );
}
