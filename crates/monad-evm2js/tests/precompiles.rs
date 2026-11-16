//! Shared known-answer precompile vectors for all supported Monad forks.

use monad_evm2js::Engine;

#[path = "support/precompiles.rs"]
mod precompiles;

#[test]
fn precompile_outputs_gas_and_failure_semantics() {
    precompiles::report();
}
