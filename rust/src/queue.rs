//! Jitter scheduler and FIFO release queue.
//!
//! Port of `enqueue_packet`, `queue_motion`, `queue_scroll`,
//! `release_scheduled_input_events`, and `calc_poll_timeout` in
//! [c/src/kloak.c]. See §3 and §4 of the behavior matrix for the exact
//! contract.
//!
//! The scheduler is deterministic given its inputs:
//!
//! - `now`: monotonic millisecond clock, exclusively provided by the caller.
//! - `max_delay`: runtime-configured upper bound on jitter (ms).
//! - `rng`: any `RandBetween` implementation. Production uses `/dev/urandom`;
//!   tests use a scripted source.
//!
//! Callers do NOT reach into the internal buffer — only through
//! `Scheduler::{enqueue_key, enqueue_button, enqueue_motion, enqueue_scroll,
//! enqueue_abs_pos, pop_due, next_deadline}`.

use crate::event::{InputPacket, Sink};
use std::collections::VecDeque;

/// Randomness source for the jitter scheduler.
pub trait RandBetween {
    /// Return a value in `[lower, upper]` inclusive. If `lower >= upper`,
    /// must return `upper` (matches C `random_between`).
    fn between(&mut self, lower: i64, upper: i64) -> i64;
}

/// Compute the jitter lower bound for the next enqueue.
///
/// Identical to `min(max(prev_release_time - now, 0), max_delay)` in C.
pub fn lower_bound(now: i64, prev_release_time: i64, max_delay: i32) -> i64 {
    let delta = prev_release_time.saturating_sub(now);
    delta.max(0).min(i64::from(max_delay))
}

/// Try to coalesce a new motion delta into an existing one.
///
/// Returns `Some((dx, dy))` if the sum fits in `i32`, else `None`.
pub fn coalesce_motion(last_dx: i32, last_dy: i32, new_dx: i32, new_dy: i32) -> Option<(i32, i32)> {
    let sx = i64::from(last_dx) + i64::from(new_dx);
    let sy = i64::from(last_dy) + i64::from(new_dy);
    if (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&sx)
        && (i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&sy)
    {
        Some((sx as i32, sy as i32))
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledPacket {
    pub sched_time: i64,
    pub packet: InputPacket,
    pub sink: Sink,
}

#[derive(Debug)]
pub struct Scheduler {
    queue: VecDeque<ScheduledPacket>,
    prev_release_time: i64,
    max_delay: i32,
}

impl Scheduler {
    pub fn new(max_delay: i32) -> Self {
        assert!(max_delay >= 0, "max_delay must be non-negative");
        Self {
            queue: VecDeque::new(),
            prev_release_time: 0,
            max_delay,
        }
    }

    pub fn max_delay(&self) -> i32 {
        self.max_delay
    }

    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// For tests only: inspect the current queue contents.
    #[cfg(test)]
    pub(crate) fn peek_all(&self) -> Vec<ScheduledPacket> {
        self.queue.iter().copied().collect()
    }

    fn enqueue(&mut self, now: i64, rng: &mut dyn RandBetween, packet: InputPacket, sink: Sink) {
        let lower = lower_bound(now, self.prev_release_time, self.max_delay);
        let delay = rng.between(lower, i64::from(self.max_delay));
        let sched_time = now.saturating_add(delay);
        self.queue.push_back(ScheduledPacket {
            sched_time,
            packet,
            sink,
        });
        self.prev_release_time = sched_time;
    }

    pub fn enqueue_key(&mut self, now: i64, rng: &mut dyn RandBetween, code: u32, pressed: bool) {
        // Keyboard keys are only meaningful on the keyboard sink.
        self.enqueue(now, rng, InputPacket::Key { code, pressed }, Sink::Kbd);
    }

    pub fn enqueue_button(
        &mut self,
        now: i64,
        rng: &mut dyn RandBetween,
        code: u32,
        pressed: bool,
        sink: Sink,
    ) {
        self.enqueue(now, rng, InputPacket::Button { code, pressed }, sink);
    }

    /// Enqueue a motion packet, coalescing into the last packet when possible.
    ///
    /// Matches C `queue_motion`: coalesce when the tail is also motion AND its
    /// `sched_time > now` AND the summed deltas fit in i32. Otherwise enqueue
    /// a fresh packet. Coalesced packets keep the original `sched_time`.
    pub fn enqueue_motion(&mut self, now: i64, rng: &mut dyn RandBetween, dx: i32, dy: i32) {
        if let Some(last) = self.queue.back_mut() {
            if let InputPacket::Motion { dx: ldx, dy: ldy } = last.packet {
                if last.sched_time > now {
                    if let Some((sx, sy)) = coalesce_motion(ldx, ldy, dx, dy) {
                        last.packet = InputPacket::Motion { dx: sx, dy: sy };
                        return;
                    }
                }
            }
        }
        // Relative motion only comes from KeyOrRel-class devices (real mice);
        // VM tablets emit AbsPos instead.
        self.enqueue(now, rng, InputPacket::Motion { dx, dy }, Sink::Kbd);
    }

    /// Enqueue a scroll packet; drops a no-op (both ticks zero).
    pub fn enqueue_scroll(
        &mut self,
        now: i64,
        rng: &mut dyn RandBetween,
        vert: i32,
        horiz: i32,
        sink: Sink,
    ) {
        if vert == 0 && horiz == 0 {
            return;
        }
        self.enqueue(now, rng, InputPacket::Scroll { vert, horiz }, sink);
    }

    /// Enqueue an absolute-position packet (VM-tablet passthrough).
    ///
    /// Timing is randomized via the same `[lower, max_delay]` jitter used
    /// for keystrokes, clicks, and scroll. To keep the cursor monotonic
    /// (never visibly stepping through stale intermediate points) we
    /// coalesce: if the tail of the queue is an AbsPos still waiting to
    /// fire, we overwrite its coordinates in place rather than appending
    /// a new packet. The tail's `sched_time` is preserved, so the next
    /// emission happens at the already-scheduled deadline with the latest
    /// known position.
    ///
    /// Net visual effect at high sample rates: the cursor updates at
    /// roughly `max_delay`-bounded intervals instead of every sample,
    /// with each update jumping directly to the current tablet position
    /// (no perceived teleport, just lower effective sample rate).
    pub fn enqueue_abs_pos(&mut self, now: i64, rng: &mut dyn RandBetween, x: i32, y: i32) {
        if let Some(last) = self.queue.back_mut() {
            if let InputPacket::AbsPos { .. } = last.packet {
                if last.sched_time > now {
                    last.packet = InputPacket::AbsPos { x, y };
                    return;
                }
            }
        }
        self.enqueue(now, rng, InputPacket::AbsPos { x, y }, Sink::Pointer);
    }

    /// Pop every packet whose `sched_time <= now` from the front of the queue,
    /// in FIFO order.
    pub fn pop_due(&mut self, now: i64) -> Vec<ScheduledPacket> {
        let mut out = Vec::new();
        while let Some(front) = self.queue.front() {
            if front.sched_time <= now {
                out.push(self.queue.pop_front().expect("front just observed"));
            } else {
                break;
            }
        }
        out
    }

    /// Deadline of the head packet, or `None` if the queue is empty.
    pub fn next_deadline(&self) -> Option<i64> {
        self.queue.front().map(|p| p.sched_time)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scripted randomness: returns the lower bound (`between(lo, hi) -> lo`).
    struct MinRng;
    impl RandBetween for MinRng {
        fn between(&mut self, lower: i64, _upper: i64) -> i64 {
            lower
        }
    }

    /// Scripted randomness: returns the upper bound.
    struct MaxRng;
    impl RandBetween for MaxRng {
        fn between(&mut self, _lower: i64, upper: i64) -> i64 {
            upper
        }
    }

    /// Scripted randomness: returns a queued series of literal values.
    struct ScriptedRng(std::collections::VecDeque<i64>);
    impl RandBetween for ScriptedRng {
        fn between(&mut self, lower: i64, upper: i64) -> i64 {
            let v = self.0.pop_front().expect("rng exhausted");
            assert!((lower..=upper).contains(&v), "scripted rng outside bounds");
            v
        }
    }

    #[test]
    fn lower_bound_behind() {
        assert_eq!(lower_bound(100, 10_000, 50), 50);
    }

    #[test]
    fn lower_bound_ahead() {
        assert_eq!(lower_bound(1000, 500, 100), 0);
    }

    #[test]
    fn lower_bound_mid() {
        assert_eq!(lower_bound(1000, 1030, 100), 30);
    }

    #[test]
    fn lower_bound_zero_max_delay() {
        assert_eq!(lower_bound(0, 9999, 0), 0);
    }

    #[test]
    fn coalesce_normal() {
        assert_eq!(coalesce_motion(5, -3, 10, 7), Some((15, 4)));
    }

    #[test]
    fn coalesce_saturation_edge() {
        assert_eq!(
            coalesce_motion(i32::MAX - 5, i32::MIN + 5, 5, -5),
            Some((i32::MAX, i32::MIN))
        );
    }

    #[test]
    fn coalesce_overflow_positive() {
        assert_eq!(coalesce_motion(i32::MAX - 1, 0, 10, 0), None);
    }

    #[test]
    fn coalesce_overflow_negative() {
        assert_eq!(coalesce_motion(0, i32::MIN + 1, 0, -10), None);
    }

    #[test]
    fn scheduler_assigns_sched_time_within_bounds() {
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_key(0, &mut rng, 30, true);
        let pkts = s.peek_all();
        assert_eq!(pkts.len(), 1);
        assert_eq!(
            pkts[0].sched_time, 100,
            "MaxRng -> sched_time = now + max_delay"
        );
    }

    #[test]
    fn scheduler_preserves_fifo_when_queue_is_behind() {
        // If prev_release_time is far in the future relative to `now`, lower
        // pins to max_delay so every new packet gets +max_delay over prev.
        let mut s = Scheduler::new(50);
        let mut rng = MinRng; // always uses `lower`
        s.enqueue_key(0, &mut rng, 30, true); // prev now = 0 + 0 (lower=0)
        s.enqueue_key(100, &mut rng, 31, true);
        // At now=100 prev=0, ahead, so lower=0 — acceptable for MinRng.
        let pkts = s.peek_all();
        assert!(pkts[1].sched_time >= pkts[0].sched_time);
    }

    #[test]
    fn scheduler_honors_prev_release_when_queue_is_behind() {
        // Force prev_release_time far beyond now: use MaxRng so first enqueue
        // sets prev to now + max_delay, then next enqueue has prev >> now.
        let mut s = Scheduler::new(50);
        let mut rng = MinRng;
        // First packet at now=0: lower=0, rng=min -> delay=0 -> sched=0.
        s.enqueue_key(0, &mut rng, 1, true);
        // Manually stage a worst-case scenario: prev = 200, now = 100, max=50.
        s.prev_release_time = 200;
        // lower = min(max(200-100, 0), 50) = 50.
        let lb = lower_bound(100, 200, 50);
        assert_eq!(lb, 50);
        s.enqueue_key(100, &mut rng, 2, true);
        let pkts = s.peek_all();
        // rng returns `lower`=50, so sched = now + 50 = 150.
        assert_eq!(pkts[1].sched_time, 150);
        // And prev_release_time advances:
        assert_eq!(s.prev_release_time, 150);
    }

    #[test]
    fn pop_due_returns_fifo_only_up_to_now() {
        let mut s = Scheduler::new(100);
        let mut rng = ScriptedRng([20, 50, 80].into_iter().collect());
        s.enqueue_key(0, &mut rng, 1, true); // sched=20
        s.enqueue_key(0, &mut rng, 2, true); // prev=20 -> lower=20, rng=50 -> sched=50
        s.enqueue_key(0, &mut rng, 3, true); // prev=50 -> lower=50, rng=80 -> sched=80
        let due = s.pop_due(55);
        let codes: Vec<u32> = due
            .iter()
            .map(|p| match p.packet {
                InputPacket::Key { code, .. } => code,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(codes, vec![1, 2]);
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn motion_coalesces_when_tail_is_pending() {
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_motion(0, &mut rng, 3, -2); // sched=100
        s.enqueue_motion(10, &mut rng, 7, 5); // tail motion, sched_time 100 > now 10
        let pkts = s.peek_all();
        assert_eq!(pkts.len(), 1);
        assert_eq!(pkts[0].packet, InputPacket::Motion { dx: 10, dy: 3 });
    }

    #[test]
    fn motion_does_not_coalesce_once_tail_is_due() {
        let mut s = Scheduler::new(100);
        let mut rng = MinRng; // first motion sched at now=0 -> sched=0 (already due)
        s.enqueue_motion(0, &mut rng, 3, -2);
        s.enqueue_motion(10, &mut rng, 7, 5);
        assert_eq!(s.queue_len(), 2);
    }

    #[test]
    fn motion_does_not_coalesce_on_overflow() {
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_motion(0, &mut rng, i32::MAX - 1, 0);
        s.enqueue_motion(10, &mut rng, 10, 0);
        assert_eq!(s.queue_len(), 2);
    }

    #[test]
    fn scroll_zero_is_dropped() {
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_scroll(0, &mut rng, 0, 0, Sink::Kbd);
        assert!(s.is_empty());
    }

    #[test]
    fn scroll_nonzero_enqueues() {
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_scroll(0, &mut rng, 1, 0, Sink::Kbd);
        assert_eq!(s.queue_len(), 1);
    }

    #[test]
    fn enqueue_key_uses_kbd_sink() {
        let mut s = Scheduler::new(100);
        let mut rng = MinRng;
        s.enqueue_key(0, &mut rng, 30, true);
        assert_eq!(s.peek_all()[0].sink, Sink::Kbd);
    }

    #[test]
    fn enqueue_motion_uses_kbd_sink() {
        let mut s = Scheduler::new(100);
        let mut rng = MinRng;
        s.enqueue_motion(0, &mut rng, 1, 1);
        assert_eq!(s.peek_all()[0].sink, Sink::Kbd);
    }

    #[test]
    fn enqueue_abs_pos_uses_pointer_sink() {
        let mut s = Scheduler::new(100);
        let mut rng = MinRng;
        s.enqueue_abs_pos(0, &mut rng, 100, 200);
        assert_eq!(s.peek_all()[0].sink, Sink::Pointer);
    }

    #[test]
    fn enqueue_button_respects_given_sink() {
        let mut s = Scheduler::new(100);
        let mut rng = MinRng;
        s.enqueue_button(0, &mut rng, 0x110, true, Sink::Pointer);
        s.enqueue_button(0, &mut rng, 0x110, true, Sink::Kbd);
        let pkts = s.peek_all();
        assert_eq!(pkts[0].sink, Sink::Pointer);
        assert_eq!(pkts[1].sink, Sink::Kbd);
    }

    #[test]
    fn next_deadline_none_when_empty() {
        let s = Scheduler::new(100);
        assert_eq!(s.next_deadline(), None);
    }

    #[test]
    fn enqueue_abs_pos_produces_one_packet() {
        let mut s = Scheduler::new(50);
        let mut rng = MinRng;
        s.enqueue_abs_pos(0, &mut rng, 1000, 2000);
        assert_eq!(s.queue_len(), 1);
        let pkts = s.pop_due(1_000_000);
        assert_eq!(pkts.len(), 1);
        match pkts[0].packet {
            InputPacket::AbsPos { x, y } => {
                assert_eq!(x, 1000);
                assert_eq!(y, 2000);
            }
            _ => panic!("expected AbsPos"),
        }
    }

    #[test]
    fn abs_pos_coalesces_while_tail_pending() {
        // With jittered timing, consecutive AbsPos samples that arrive
        // before the tail's scheduled emission are merged: the tail keeps
        // its sched_time but adopts the latest (x, y). This keeps the
        // cursor monotonic — it jumps straight to the newest position
        // when the deadline hits, never stepping through stale samples.
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng; // first sched = now + 100 = 100
        s.enqueue_abs_pos(0, &mut rng, 100, 100);
        s.enqueue_abs_pos(10, &mut rng, 200, 200);
        assert_eq!(s.queue_len(), 1, "second sample must coalesce into tail");
        let pkts = s.peek_all();
        assert_eq!(pkts[0].packet, InputPacket::AbsPos { x: 200, y: 200 });
        assert_eq!(pkts[0].sched_time, 100, "tail sched_time is preserved");
    }

    #[test]
    fn abs_pos_is_jittered_like_keystrokes() {
        // Per-sample sched_time must lie within [now, now + max_delay],
        // same contract as keystroke timing anonymization.
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_abs_pos(0, &mut rng, 50, 50);
        let pkts = s.peek_all();
        assert_eq!(pkts[0].sched_time, 100, "MaxRng -> now + max_delay");

        let mut s2 = Scheduler::new(100);
        let mut rng2 = MinRng;
        s2.enqueue_abs_pos(0, &mut rng2, 50, 50);
        let pkts2 = s2.peek_all();
        assert_eq!(pkts2[0].sched_time, 0, "MinRng at fresh queue -> now");
    }

    #[test]
    fn abs_pos_respects_prev_release_ratchet() {
        // A keystroke enqueued before an AbsPos must push the AbsPos's
        // lower bound forward, matching the global FIFO ratchet.
        let mut s = Scheduler::new(100);
        let mut rng = MaxRng;
        s.enqueue_key(0, &mut rng, 30, true); // sched=100, prev=100
                                              // now=10: lower = min(max(100-10,0), 100) = 90. MaxRng picks 100.
        s.enqueue_abs_pos(10, &mut rng, 50, 50);
        let pkts = s.peek_all();
        assert_eq!(pkts[1].sched_time, 110);
    }
}
