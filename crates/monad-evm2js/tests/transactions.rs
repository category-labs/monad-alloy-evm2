//! Native typed-transaction, authorization and reserve-context regressions.

use monad_evm2js::Engine;

#[path = "support/transactions.rs"]
mod transactions;

#[test]
fn typed_transactions_authorizations_and_reserve_context() {
    transactions::report();
}
