//! Corresponding hardforks in the two execution engines.

use monad_evm2::MonadHardfork;
use monad_revm::MonadHardfork as ReferenceHardfork;

pub(crate) const FORKS: [(MonadHardfork, ReferenceHardfork); 4] = [
    (MonadHardfork::MonadEight, ReferenceHardfork::MonadEight),
    (MonadHardfork::MonadNine, ReferenceHardfork::MonadNine),
    (MonadHardfork::MonadTen, ReferenceHardfork::MonadTen),
    (MonadHardfork::MonadNext, ReferenceHardfork::MonadNext),
];
