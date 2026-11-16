//! Emit native typed-transaction results for comparison with the installed WASM package.

use monad_evm2js::Engine;

#[path = "../tests/support/transactions.rs"]
mod transactions;

fn main() {
    println!("{}", transactions::report());
}
