//! Emit native execution results for comparison with the installed WASM package.

use monad_evm2js::Engine;

#[path = "../tests/support/precompiles.rs"]
mod precompiles;

fn main() {
    println!("{}", precompiles::report());
}
