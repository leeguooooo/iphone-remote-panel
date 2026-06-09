/// Control-lease arbitration for the single shared cursor/input stream.
///
/// # Why this exists
///
/// On macOS, input is injected via the global HID event tap and drives the host's ONE real
/// cursor with the Mirroring window frontmost.  A human controller (iPhone Safari) and agent
/// controllers (Hermes via MCP) can both want to drive it.  This module grants that single
/// cursor to **one controller at a time**.  Without the lease, two controllers corrupt each
/// other's gestures fighting over one cursor.
///
/// # Preemption policy
///
/// Any holder can be preempted by any other holder — human-by-human, agent-by-agent,
/// human-by-agent, or agent-by-human.  The `preempt` method synthesises cleanup
/// `ReleaseEvent`s for whatever the displaced holder still had pressed, so the caller can
/// inject them **before** the new holder begins acting; this prevents stuck pointers/keys.
///
/// # Clock independence
///
/// All methods that care about time accept `now: u64` (unix seconds) as a parameter.
/// There are **no** `std::time` calls inside this module.

// ─────────────────────────────────────────────────────────────────────────────
// Public types
// ─────────────────────────────────────────────────────────────────────────────

/// Identifies who holds the control lease.
#[derive(PartialEq, Clone, Debug)]
pub enum Holder {
    /// A human operator, identified by their WebRTC session ID.
    Human(String),
    /// An automated agent, identified by its agent ID.
    Agent(String),
}

/// Tracks what the active holder currently has physically "held down."
/// Used by `preempt` / `tick` to synthesise cleanup events.
#[derive(Clone, Debug)]
pub enum Pressed {
    /// A pointer (mouse/touch) held at a logical coordinate.
    Pointer { x: f64, y: f64 },
    /// A key (key-code string, e.g. `"KeyA"` / `"ArrowLeft"`) held down.
    Key(String),
}

/// A cleanup event the **caller** must inject (via the event tap) when a holder
/// is displaced while still pressing something.  Injecting these before the new
/// holder acts ensures no cursor or key is left stuck.
#[derive(PartialEq, Debug)]
pub enum ReleaseEvent {
    /// Synthesised pointer-up at the last-known held coordinates.
    PointerUp { x: f64, y: f64 },
    /// Synthesised key-up for the given key code.
    KeyUp(String),
}

/// An opaque handle returned by `Control::acquire`.  A gesture loop should call
/// `Control::is_current` with this handle before every injected event; if it
/// returns `false` the lease has been preempted and the loop must abort.
#[derive(Clone, Debug, PartialEq)]
pub struct Lease {
    pub holder: Holder,
    pub generation: u64,
}

// ─────────────────────────────────────────────────────────────────────────────
// State machine
// ─────────────────────────────────────────────────────────────────────────────

/// The control-lease state machine.  One instance lives per active session.
///
/// All mutating operations are synchronous (`&mut self`).  The caller is
/// responsible for serialising access (e.g. a `Mutex<Control>` in the server).
pub struct Control {
    holder: Option<Holder>,
    generation: u64,
    last_active: u64,
    pressed: Vec<Pressed>,
}

impl Control {
    /// Create a new, idle `Control` (no holder).
    pub fn new() -> Self {
        Control {
            holder: None,
            generation: 0,
            last_active: 0,
            pressed: Vec::new(),
        }
    }

    // ── Acquire ──────────────────────────────────────────────────────────────

    /// Acquire the control lease for `who`.
    ///
    /// Behaviour:
    /// - If the slot is **free**, `who` takes it immediately.
    /// - If `who` is **already the holder** (same variant and ID), this acts as a
    ///   touch/refresh: the generation is still bumped (invalidating any outstanding
    ///   `Lease` handles), `last_active` is updated, but `pressed` is cleared.
    /// - If a **different** holder currently holds the lease, `who` preempts them.
    ///   Pressed state is cleared; cleanup events are **discarded** (use `preempt`
    ///   instead when you need the cleanup events for injection).
    ///
    /// In all cases a fresh `Lease` is returned.
    pub fn acquire(&mut self, who: Holder, now: u64) -> Lease {
        self.generation += 1;
        self.holder = Some(who.clone());
        self.last_active = now;
        self.pressed.clear();
        Lease {
            holder: who,
            generation: self.generation,
        }
    }

    // ── Preempt ───────────────────────────────────────────────────────────────

    /// Forcibly take control for `who`, returning the cleanup events the caller
    /// must inject for whatever the displaced holder still had pressed.
    ///
    /// The caller **must** inject the returned `ReleaseEvent`s (in order) before
    /// letting the new holder act, so no pointer or key is left stuck.
    pub fn preempt(&mut self, who: Holder, now: u64) -> (Lease, Vec<ReleaseEvent>) {
        let releases = self.drain_pressed_as_releases();
        let lease = self.acquire(who, now);
        (lease, releases)
    }

    // ── Validity check ────────────────────────────────────────────────────────

    /// Returns `true` iff `lease` still corresponds to the current holder and
    /// generation.  A gesture loop should call this before every injected event
    /// and abort if it returns `false`.
    pub fn is_current(&self, lease: &Lease) -> bool {
        match &self.holder {
            Some(h) => *h == lease.holder && self.generation == lease.generation,
            None => false,
        }
    }

    // ── Press tracking ────────────────────────────────────────────────────────

    /// Record that `lease`'s holder is now pressing `p`.
    /// No-op if `lease` is stale.
    pub fn record_press(&mut self, lease: &Lease, p: Pressed) {
        if self.is_current(lease) {
            self.pressed.push(p);
        }
    }

    /// Record that `lease`'s holder has released a previously-pressed item.
    /// Removes the **first** matching entry so `preempt` does not double-emit it.
    /// No-op if `lease` is stale.
    pub fn record_release(&mut self, lease: &Lease, p: &Pressed) {
        if !self.is_current(lease) {
            return;
        }
        // Remove the first entry that matches p.
        if let Some(idx) = self.pressed.iter().position(|x| pressed_eq(x, p)) {
            self.pressed.remove(idx);
        }
    }

    // ── Activity touch ────────────────────────────────────────────────────────

    /// Update `last_active` for the current holder (rate-limit calls in the
    /// gesture loop).  No-op if `lease` is stale.
    pub fn touch(&mut self, lease: &Lease, now: u64) {
        if self.is_current(lease) {
            self.last_active = now;
        }
    }

    // ── Explicit release ──────────────────────────────────────────────────────

    /// Explicitly release the lease.  No-op if `lease` is stale (already
    /// preempted or released).
    pub fn release(&mut self, lease: &Lease) {
        if self.is_current(lease) {
            self.holder = None;
            self.pressed.clear();
            // Note: we do NOT bump the generation here; the lease is simply
            // invalidated by clearing the holder (is_current returns false
            // when holder is None).
        }
    }

    // ── Idle timeout ─────────────────────────────────────────────────────────

    /// Advance the timeout clock.  If the current holder has been idle for
    /// `>= idle_secs` seconds, auto-release them and return cleanup events for
    /// anything still pressed.  Returns an empty `Vec` if no one holds the lease
    /// or the timeout has not yet elapsed.
    ///
    /// Typical caller: pass `idle_secs = 30`.
    pub fn tick(&mut self, now: u64, idle_secs: u64) -> Vec<ReleaseEvent> {
        if self.holder.is_none() {
            return Vec::new();
        }
        if now.saturating_sub(self.last_active) >= idle_secs {
            let releases = self.drain_pressed_as_releases();
            self.holder = None;
            self.pressed.clear(); // already drained, but be explicit
            return releases;
        }
        Vec::new()
    }

    // ── Private helpers ───────────────────────────────────────────────────────

    /// Convert the current `pressed` list to `ReleaseEvent`s without modifying
    /// `self.generation` or `self.holder`.
    fn drain_pressed_as_releases(&self) -> Vec<ReleaseEvent> {
        self.pressed
            .iter()
            .map(|p| match p {
                Pressed::Pointer { x, y } => ReleaseEvent::PointerUp { x: *x, y: *y },
                Pressed::Key(k) => ReleaseEvent::KeyUp(k.clone()),
            })
            .collect()
    }
}

impl Default for Control {
    fn default() -> Self {
        Self::new()
    }
}

/// Structural equality for `Pressed` (used by `record_release`).
fn pressed_eq(a: &Pressed, b: &Pressed) -> bool {
    match (a, b) {
        (Pressed::Pointer { x: ax, y: ay }, Pressed::Pointer { x: bx, y: by }) => {
            ax == bx && ay == by
        }
        (Pressed::Key(ak), Pressed::Key(bk)) => ak == bk,
        _ => false,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const T0: u64 = 1_000_000;

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn human(id: &str) -> Holder {
        Holder::Human(id.to_string())
    }

    fn agent(id: &str) -> Holder {
        Holder::Agent(id.to_string())
    }

    // ── Acquire on free slot ─────────────────────────────────────────────────

    #[test]
    fn acquire_on_free_returns_valid_lease() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        assert!(c.is_current(&lease));
        assert_eq!(lease.generation, 1);
    }

    // ── Same holder re-acquires ───────────────────────────────────────────────

    #[test]
    fn same_holder_reacquire_still_current_but_bumps_generation() {
        let mut c = Control::new();
        let lease1 = c.acquire(human("s1"), T0);
        let lease2 = c.acquire(human("s1"), T0 + 1);
        // Old lease is stale (generation changed).
        assert!(!c.is_current(&lease1));
        // New lease is valid.
        assert!(c.is_current(&lease2));
        assert!(lease2.generation > lease1.generation);
    }

    // ── Agent preempted by human ──────────────────────────────────────────────

    #[test]
    fn agent_preempted_by_human_bumps_generation_and_invalidates_agent_lease() {
        let mut c = Control::new();
        let agent_lease = c.acquire(agent("hermes"), T0);
        assert!(c.is_current(&agent_lease));

        let human_lease = c.acquire(human("s1"), T0 + 1);
        // Agent's old lease is now stale.
        assert!(!c.is_current(&agent_lease));
        // Human's new lease is valid.
        assert!(c.is_current(&human_lease));
    }

    // ── is_current false after any generation bump ────────────────────────────

    #[test]
    fn is_current_false_after_generation_bump() {
        let mut c = Control::new();
        let lease1 = c.acquire(human("s1"), T0);
        // Any subsequent acquire (even same holder) invalidates the old lease.
        let _lease2 = c.acquire(human("s1"), T0 + 5);
        assert!(!c.is_current(&lease1));
    }

    // ── preempt returns PointerUp for held pointer ────────────────────────────

    #[test]
    fn preempt_with_pointer_down_returns_pointer_up() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        c.record_press(&lease, Pressed::Pointer { x: 42.0, y: 99.5 });

        let (new_lease, events) = c.preempt(agent("hermes"), T0 + 1);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ReleaseEvent::PointerUp { x: 42.0, y: 99.5 });
        assert!(c.is_current(&new_lease));
        // Old lease invalidated.
        assert!(!c.is_current(&lease));
    }

    // ── preempt returns KeyUp for held key ────────────────────────────────────

    #[test]
    fn preempt_with_key_down_returns_key_up() {
        let mut c = Control::new();
        let lease = c.acquire(agent("hermes"), T0);
        c.record_press(&lease, Pressed::Key("ArrowLeft".to_string()));

        let (new_lease, events) = c.preempt(human("s1"), T0 + 1);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0], ReleaseEvent::KeyUp("ArrowLeft".to_string()));
        assert!(c.is_current(&new_lease));
    }

    // ── preempt returns multiple events for multiple held inputs ──────────────

    #[test]
    fn preempt_with_pointer_and_key_returns_both() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        c.record_press(&lease, Pressed::Pointer { x: 10.0, y: 20.0 });
        c.record_press(&lease, Pressed::Key("KeyA".to_string()));

        let (_new_lease, events) = c.preempt(agent("bot"), T0 + 1);

        assert_eq!(events.len(), 2);
        assert_eq!(events[0], ReleaseEvent::PointerUp { x: 10.0, y: 20.0 });
        assert_eq!(events[1], ReleaseEvent::KeyUp("KeyA".to_string()));
    }

    // ── preempt with nothing pressed returns no events ────────────────────────

    #[test]
    fn preempt_with_nothing_pressed_returns_empty_events() {
        let mut c = Control::new();
        let _lease = c.acquire(human("s1"), T0);
        let (_new_lease, events) = c.preempt(agent("hermes"), T0 + 1);
        assert!(events.is_empty());
    }

    // ── One human controller rule: second human preempts first ────────────────

    #[test]
    fn second_different_human_preempts_first_human() {
        let mut c = Control::new();
        let lease_a = c.acquire(human("alice"), T0);
        c.record_press(&lease_a, Pressed::Pointer { x: 5.0, y: 5.0 });

        let (lease_b, events) = c.preempt(human("bob"), T0 + 1);

        // Alice is displaced and her pointer is cleaned up.
        assert!(!c.is_current(&lease_a));
        assert!(c.is_current(&lease_b));
        assert_eq!(events, vec![ReleaseEvent::PointerUp { x: 5.0, y: 5.0 }]);
    }

    // ── Idle tick auto-releases and returns pending releases ──────────────────

    #[test]
    fn tick_past_idle_timeout_auto_releases_with_cleanup() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        c.record_press(&lease, Pressed::Key("KeyZ".to_string()));

        // Advance by exactly `idle_secs` — should trigger.
        let events = c.tick(T0 + 30, 30);
        assert_eq!(events, vec![ReleaseEvent::KeyUp("KeyZ".to_string())]);
        // Holder is cleared — lease is stale.
        assert!(!c.is_current(&lease));
    }

    #[test]
    fn tick_before_idle_timeout_does_nothing() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        let events = c.tick(T0 + 29, 30);
        assert!(events.is_empty());
        assert!(c.is_current(&lease));
    }

    #[test]
    fn tick_when_no_holder_returns_empty() {
        let mut c = Control::new();
        let events = c.tick(T0 + 100, 30);
        assert!(events.is_empty());
    }

    // ── record_release removes entry so preempt does not double-emit ──────────

    #[test]
    fn record_release_removes_pending_release_so_preempt_does_not_double_emit() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        c.record_press(&lease, Pressed::Key("KeyM".to_string()));
        // Holder properly releases the key before preempt occurs.
        c.record_release(&lease, &Pressed::Key("KeyM".to_string()));

        let (_new_lease, events) = c.preempt(agent("hermes"), T0 + 1);
        // No KeyUp should appear — it was already released.
        assert!(events.is_empty());
    }

    // ── record_press / record_release no-op on stale lease ───────────────────

    #[test]
    fn record_press_no_op_on_stale_lease() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        // Preempt to invalidate the lease.
        let _ = c.preempt(agent("hermes"), T0 + 1);
        // Attempting to record a press with the stale lease is silently ignored.
        c.record_press(&lease, Pressed::Pointer { x: 0.0, y: 0.0 });
        let (_, events) = c.preempt(human("s1"), T0 + 2);
        assert!(events.is_empty());
    }

    // ── Explicit release ──────────────────────────────────────────────────────

    #[test]
    fn explicit_release_clears_holder() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        c.release(&lease);
        assert!(!c.is_current(&lease));
    }

    #[test]
    fn explicit_release_no_op_on_stale_lease() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        let _lease2 = c.acquire(agent("hermes"), T0 + 1);
        // Calling release on the displaced holder's stale lease must not affect
        // the current holder.
        c.release(&lease);
        assert!(c.is_current(&_lease2));
    }

    // ── touch updates last_active ─────────────────────────────────────────────

    #[test]
    fn touch_defers_idle_timeout() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        // Touch at T0+25, resetting the idle clock.
        c.touch(&lease, T0 + 25);
        // T0+25+29 = T0+54 — still 29 s since last touch, below 30 s threshold.
        let events = c.tick(T0 + 54, 30);
        assert!(events.is_empty());
        assert!(c.is_current(&lease));
    }

    // ── acquire clears pressed (no leftovers from displaced holder) ───────────

    #[test]
    fn acquire_clears_pressed_of_displaced_holder() {
        let mut c = Control::new();
        let lease = c.acquire(human("s1"), T0);
        c.record_press(&lease, Pressed::Pointer { x: 1.0, y: 2.0 });
        // A direct acquire (not preempt) displaces the holder silently.
        let new_lease = c.acquire(agent("hermes"), T0 + 1);
        // No cleanup events from a subsequent preempt of the new holder
        // (they pressed nothing yet).
        let (_, events) = c.preempt(human("s1"), T0 + 2);
        assert!(events.is_empty());
        let _ = new_lease; // suppress unused warning
    }
}
