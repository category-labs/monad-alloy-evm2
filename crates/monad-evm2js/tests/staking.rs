//! Native staking lifecycle and system-transition regression tests.

use monad_evm2js::Engine;

#[path = "support/staking.rs"]
mod staking;

#[test]
fn staking_lifecycle_and_atomic_system_transitions() {
    staking::report();
}
