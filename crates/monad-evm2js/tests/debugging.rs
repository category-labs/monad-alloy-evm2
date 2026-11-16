//! Nested execution and child-contract debugging through the public API.
use monad_evm2js::Engine;
#[path = "support/debugging.rs"]
mod debugging;

#[test]
fn nested_calls_failures_and_child_creation() {
    debugging::report();
}
