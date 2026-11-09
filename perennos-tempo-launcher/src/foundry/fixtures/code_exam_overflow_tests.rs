#[cfg(test)]
mod code_exam_tests {
    use super::*;
    use std::sync::atomic::Ordering;

    #[test]
    fn tokens_saturate_and_hit_max_budget() {
        let meter = Meter::root(Budget {
            tokens: u64::MAX,
            ..Budget::unlimited()
        });

        meter.charge_tokens(u64::MAX - 1);
        meter.charge_tokens(3);

        assert_eq!(meter.used().tokens, u64::MAX);
        assert_eq!(meter.halt(), Some(Halt::Hit(Limit::Tokens)));
    }

    #[test]
    fn cloud_cents_saturate_and_hit_max_budget() {
        let meter = Meter::root(Budget {
            cloud_cents: u64::MAX,
            ..Budget::unlimited()
        });

        meter.charge_cents(u64::MAX - 1);
        meter.charge_cents(3);

        assert_eq!(meter.used().cloud_cents, u64::MAX);
        assert_eq!(meter.halt(), Some(Halt::Hit(Limit::CloudCents)));
    }

    #[test]
    fn tool_counter_at_max_stays_saturated_and_stops() {
        let meter = Meter::root(Budget {
            tool_calls: u64::MAX,
            ..Budget::unlimited()
        });
        meter.0.tools.store(u64::MAX, Ordering::SeqCst);

        meter.charge_tool();

        assert_eq!(meter.used().tool_calls, u64::MAX);
        assert_eq!(meter.halt(), Some(Halt::Hit(Limit::ToolCalls)));
    }

    #[test]
    fn child_and_parent_saturate_and_child_sees_parent_halt() {
        let root = Meter::root(Budget {
            tokens: u64::MAX,
            ..Budget::unlimited()
        });
        let child = root.child(Budget::unlimited());
        root.0.tokens.store(u64::MAX - 1, Ordering::SeqCst);
        child.0.tokens.store(u64::MAX - 1, Ordering::SeqCst);

        child.charge_tokens(3);

        assert_eq!(root.used().tokens, u64::MAX);
        assert_eq!(child.used().tokens, u64::MAX);
        assert_eq!(child.halt(), Some(Halt::Hit(Limit::Tokens)));
    }

    #[test]
    fn concurrent_charges_keep_all_normal_counters_exact() {
        const THREADS: usize = 4;
        const CHARGES_PER_THREAD: usize = 500;

        let root = Meter::root(Budget::unlimited());
        let children: Vec<_> = (0..THREADS)
            .map(|_| root.child(Budget::unlimited()))
            .collect();

        std::thread::scope(|scope| {
            for child in &children {
                let child = child.clone();
                scope.spawn(move || {
                    for _ in 0..CHARGES_PER_THREAD {
                        child.charge_tokens(1);
                        child.charge_cents(2);
                        child.charge_tool();
                    }
                });
            }
        });

        let total = (THREADS * CHARGES_PER_THREAD) as u64;
        let root_used = root.used();
        assert_eq!(root_used.tokens, total);
        assert_eq!(root_used.cloud_cents, total * 2);
        assert_eq!(root_used.tool_calls, total);

        for child in children {
            let used = child.used();
            assert_eq!(used.tokens, CHARGES_PER_THREAD as u64);
            assert_eq!(used.cloud_cents, (CHARGES_PER_THREAD * 2) as u64);
            assert_eq!(used.tool_calls, CHARGES_PER_THREAD as u64);
        }
    }
}
