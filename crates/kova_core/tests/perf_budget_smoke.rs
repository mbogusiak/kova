//! Headless integration smoke test for Kova IDE performance budgets.

use std::time::Duration;

use kova_core::perf::{
    BudgetKind, assert_within_budget, budgets, check_duration_budget, check_rss_budget,
    current_process_rss_bytes,
};

#[test]
fn headless_process_rss_is_well_below_budget() {
    let rss_bytes = current_process_rss_bytes().expect("RSS probe should succeed on macOS/Linux");
    assert!(rss_bytes > 0);

    let report = check_rss_budget(rss_bytes);
    assert_within_budget(&report);
}

#[test]
fn duration_budgets_validate_expected_envelopes() {
    let cold_start = check_duration_budget(
        BudgetKind::ColdStart,
        Duration::from_millis(50),
        budgets::COLD_START_MAX,
    );
    assert_within_budget(&cold_start);

    let input_latency = check_duration_budget(
        BudgetKind::InputLatency,
        Duration::from_micros(500),
        budgets::INPUT_LATENCY_MAX,
    );
    assert_within_budget(&input_latency);

    let go_to_class = check_duration_budget(
        BudgetKind::GoToClassReady,
        Duration::from_millis(400),
        budgets::GO_TO_CLASS_READY_MAX,
    );
    assert_within_budget(&go_to_class);
}
