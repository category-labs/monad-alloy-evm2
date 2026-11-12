//! Hardforks exercised by protocol tests and optional reference comparisons.

use monad_evm2::MonadHardfork;

#[allow(dead_code)]
pub(crate) const FORKS: [MonadHardfork; 4] = MonadHardfork::ALL;

#[cfg(feature = "compare-monad-revm")]
#[allow(dead_code)]
pub(crate) fn reference_spec(spec: MonadHardfork) -> monad_revm::MonadHardfork {
    spec.to_string().parse().expect("known reference hardfork")
}
