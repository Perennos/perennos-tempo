//! Budgets and the stop switch (std only). One copy of this file lives in the agent and one in
//! the launcher's Conductor; they are the same bytes.
//!
//! A `Meter` counts what a run uses: tokens, tool calls, cloud cents and wall time. Every member
//! of a run tree holds a child of its parent's meter, so what a member uses is added all the way
//! up, and a limit or a stop anywhere above ends everything below. Counters are atomics, so
//! members on other threads share them without a lock.
//!
//! A limit of 0 means no limit on that line. Checking happens between steps: a request already
//! on its way to a model is not interrupted, so a run can pass a limit by one step.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What one run may use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub tokens: u64,
    pub wall_time: Duration,
    pub tool_calls: u64,
    pub cloud_cents: u64,
}

impl Default for Budget {
    /// Room for a big job on a local model, and a small cap on anything that costs money.
    fn default() -> Budget {
        Budget {
            tokens: 400_000,
            wall_time: Duration::from_secs(30 * 60),
            tool_calls: 200,
            cloud_cents: 50,
        }
    }
}

impl Budget {
    pub fn unlimited() -> Budget {
        Budget {
            tokens: 0,
            wall_time: Duration::ZERO,
            tool_calls: 0,
            cloud_cents: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    Tokens,
    WallTime,
    ToolCalls,
    CloudCents,
}

impl Limit {
    pub fn name(self) -> &'static str {
        match self {
            Limit::Tokens => "token",
            Limit::WallTime => "time",
            Limit::ToolCalls => "tool-call",
            Limit::CloudCents => "cloud-spend",
        }
    }
}

/// Why a run has to end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The owner pressed stop.
    Stopped,
    /// This limit ran out.
    Hit(Limit),
}

/// What a meter has counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Used {
    pub tokens: u64,
    pub tool_calls: u64,
    pub cloud_cents: u64,
    pub secs: u64,
}

impl Halt {
    /// One plain line: which limit, and what got done (`done` is the caller's own words).
    pub fn line(self, budget: &Budget, used: &Used, done: &str) -> String {
        let what = match self {
            Halt::Stopped => "Stopped by you.".to_string(),
            Halt::Hit(Limit::Tokens) => format!(
                "Stopped at the token limit ({} of {} used).",
                used.tokens, budget.tokens
            ),
            Halt::Hit(Limit::WallTime) => format!(
                "Stopped at the time limit ({} s of {} s).",
                used.secs,
                budget.wall_time.as_secs()
            ),
            Halt::Hit(Limit::ToolCalls) => format!(
                "Stopped at the tool-call limit ({} of {} used).",
                used.tool_calls, budget.tool_calls
            ),
            Halt::Hit(Limit::CloudCents) => format!(
                "Stopped at the cloud-spend limit ({} of {} cents used).",
                used.cloud_cents, budget.cloud_cents
            ),
        };
        if done.trim().is_empty() {
            what
        } else {
            format!("{what} {}", done.trim())
        }
    }
}

impl Node {
    fn used(&self) -> Used {
        Used {
            tokens: self.tokens.load(Ordering::SeqCst),
            tool_calls: self.tools.load(Ordering::SeqCst),
            cloud_cents: self.cents.load(Ordering::SeqCst),
            secs: self.started.elapsed().as_secs(),
        }
    }
}

struct Node {
    budget: Budget,
    started: Instant,
    tokens: AtomicU64,
    tools: AtomicU64,
    cents: AtomicU64,
    cancel: AtomicBool,
    parent: Option<Arc<Node>>,
}

/// The counters and the stop flag of one member, linked to its parent's.
#[derive(Clone)]
pub struct Meter(Arc<Node>);

impl Meter {
    fn make(budget: Budget, parent: Option<Arc<Node>>) -> Meter {
        Meter(Arc::new(Node {
            budget,
            started: Instant::now(),
            tokens: AtomicU64::new(0),
            tools: AtomicU64::new(0),
            cents: AtomicU64::new(0),
            cancel: AtomicBool::new(false),
            parent,
        }))
    }

    /// The top of a run tree.
    pub fn root(budget: Budget) -> Meter {
        Meter::make(budget, None)
    }

    /// A member's meter: its own limits, and everything it counts also counts above it.
    pub fn child(&self, budget: Budget) -> Meter {
        Meter::make(budget, Some(self.0.clone()))
    }

    fn chain(&self) -> impl Iterator<Item = &Node> {
        let mut at = Some(&*self.0);
        std::iter::from_fn(move || {
            let n = at?;
            at = n.parent.as_deref();
            Some(n)
        })
    }

    pub fn charge_tokens(&self, n: u64) {
        self.chain().for_each(|x| {
            x.tokens.fetch_add(n, Ordering::SeqCst);
        });
    }

    pub fn charge_tool(&self) {
        self.chain().for_each(|x| {
            x.tools.fetch_add(1, Ordering::SeqCst);
        });
    }

    pub fn charge_cents(&self, n: u64) {
        self.chain().for_each(|x| {
            x.cents.fetch_add(n, Ordering::SeqCst);
        });
    }

    /// The stop switch: this member and everything below it end at their next check. Stopping
    /// the root ends the whole tree.
    pub fn stop(&self) {
        self.0.cancel.store(true, Ordering::SeqCst);
    }

    /// Why this member has to end now, if it does: a stop or a spent limit here or above.
    pub fn halt(&self) -> Option<Halt> {
        self.check().map(|(h, _)| h)
    }

    /// `halt`, and the member whose limit it was (a child can be stopped by its parent's).
    fn check(&self) -> Option<(Halt, &Node)> {
        for n in self.chain() {
            if n.cancel.load(Ordering::SeqCst) {
                return Some((Halt::Stopped, n));
            }
            let b = &n.budget;
            let spent = |used: u64, max: u64| max > 0 && used >= max;
            if spent(n.tokens.load(Ordering::SeqCst), b.tokens) {
                return Some((Halt::Hit(Limit::Tokens), n));
            }
            if spent(n.tools.load(Ordering::SeqCst), b.tool_calls) {
                return Some((Halt::Hit(Limit::ToolCalls), n));
            }
            if spent(n.cents.load(Ordering::SeqCst), b.cloud_cents) {
                return Some((Halt::Hit(Limit::CloudCents), n));
            }
            if !b.wall_time.is_zero() && n.started.elapsed() >= b.wall_time {
                return Some((Halt::Hit(Limit::WallTime), n));
            }
        }
        None
    }

    /// What this member's subtree has used so far.
    pub fn used(&self) -> Used {
        self.0.used()
    }

    pub fn budget(&self) -> Budget {
        self.0.budget
    }

    /// The plain line for a halt on this meter. A limit that stopped it from above is told with
    /// that member's figures, not this one's.
    pub fn halt_line(&self, h: Halt, done: &str) -> String {
        match self.check() {
            Some((_, n)) => h.line(&n.budget, &n.used(), done),
            None => h.line(&self.0.budget, &self.used(), done),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Budget {
        Budget {
            tokens: 100,
            wall_time: Duration::ZERO,
            tool_calls: 2,
            cloud_cents: 0,
        }
    }

    #[test]
    fn a_limit_ends_the_run_and_says_which() {
        let m = Meter::root(small());
        assert_eq!(m.halt(), None);
        m.charge_tool();
        m.charge_tool();
        let h = m.halt().unwrap();
        assert_eq!(h, Halt::Hit(Limit::ToolCalls));
        let line = m.halt_line(h, "Two of three parts are done.");
        assert!(line.starts_with("Stopped at the tool-call limit (2 of 2 used)."));
        assert!(line.ends_with("Two of three parts are done."));
    }

    #[test]
    fn what_a_child_uses_counts_above_and_a_parent_limit_stops_the_child() {
        let root = Meter::root(small());
        let a = root.child(Budget::unlimited());
        let b = root.child(Budget::unlimited());
        a.charge_tokens(60);
        b.charge_tokens(60);
        assert_eq!(root.used().tokens, 120);
        assert_eq!(a.used().tokens, 60);
        // the children have no limit of their own; the root's runs out for both
        assert_eq!(a.halt(), Some(Halt::Hit(Limit::Tokens)));
        assert_eq!(b.halt(), Some(Halt::Hit(Limit::Tokens)));
    }

    #[test]
    fn a_limit_from_above_is_told_with_the_figures_of_the_member_that_set_it() {
        let root = Meter::root(small());
        let kid = root.child(Budget::unlimited());
        kid.charge_tool();
        kid.charge_tool();
        let h = kid.halt().unwrap();
        assert!(kid
            .halt_line(h, "")
            .starts_with("Stopped at the tool-call limit (2 of 2 used)."));
    }

    #[test]
    fn one_stop_at_the_top_ends_every_member_and_a_child_stop_stays_below() {
        let root = Meter::root(Budget::unlimited());
        let a = root.child(Budget::unlimited());
        let grand = a.child(Budget::unlimited());
        let b = root.child(Budget::unlimited());
        a.stop();
        assert_eq!(grand.halt(), Some(Halt::Stopped));
        assert_eq!(root.halt(), None);
        assert_eq!(b.halt(), None);
        root.stop();
        assert_eq!(b.halt(), Some(Halt::Stopped));
    }

    #[test]
    fn time_and_money_are_limits_too_and_zero_means_none() {
        let t = Meter::root(Budget {
            wall_time: Duration::from_millis(1),
            ..Budget::unlimited()
        });
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(t.halt(), Some(Halt::Hit(Limit::WallTime)));
        let c = Meter::root(Budget {
            cloud_cents: 3,
            ..Budget::unlimited()
        });
        c.charge_cents(3);
        assert_eq!(c.halt(), Some(Halt::Hit(Limit::CloudCents)));
        let free = Meter::root(Budget::unlimited());
        free.charge_tokens(u64::MAX / 2);
        assert_eq!(free.halt(), None);
    }

    #[test]
    fn members_on_other_threads_share_the_counters() {
        let root = Meter::root(Budget::unlimited());
        std::thread::scope(|s| {
            for _ in 0..4 {
                let m = root.child(Budget::unlimited());
                s.spawn(move || (0..250).for_each(|_| m.charge_tokens(1)));
            }
        });
        assert_eq!(root.used().tokens, 1000);
    }
}
