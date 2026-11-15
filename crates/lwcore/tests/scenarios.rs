//! Integration test: runs the portable scenario suite natively.
//!
//! The same suite also runs on wasm32-unknown-unknown through the
//! `wasmtest` crate + `tools/wasm-test.cjs`, proving the core stays
//! deterministic on a second platform.

use lwcore::tests;

#[test]
fn all_scenarios_pass() {
    let failures = tests::run_tests();
    assert!(
        failures.is_empty(),
        "scenario failures:\n  {}",
        failures.join("\n  ")
    );
}

#[test]
fn scenario_list_is_the_expected_seven() {
    assert_eq!(
        tests::SCENARIOS,
        &[
            "match_invariants",
            "cursor_wall_clamp",
            "cursor_slides_along_walls",
            "army_converges",
            "deterministic_multi_team",
            "lockstep_loopback_through_protocol",
            "front_line_battles",
        ]
    );
}
