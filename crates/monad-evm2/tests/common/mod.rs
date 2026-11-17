//! Hardforks exercised by protocol tests.

use monad_evm2::MonadHardfork;

#[allow(dead_code)]
pub(crate) const FORKS: [MonadHardfork; 4] = MonadHardfork::ALL;
