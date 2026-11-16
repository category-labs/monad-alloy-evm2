//! Emit native staking results for comparison with the installed WASM package.

use monad_evm2js::Engine;

#[path = "../tests/support/staking.rs"]
mod staking;

fn main() {
    println!("{}", staking::report());
}
