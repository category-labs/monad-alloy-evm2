//! Paired snapshots of native state and Monad transaction bookkeeping.

use super::sync_reserve_balance;
use crate::{MonadEvmExt, MonadHost};
use evm2::{
    DatabaseError,
    evm::{DynDatabase, StateCheckpoint, StateSnapshot},
};

/// A reversible frame boundary, including reserves and the suspended memory pool.
///
/// Native state already journals storage-page tracking through transient storage.
/// Use this only with the same state journal; use a full snapshot across forks.
#[derive(Clone, Debug)]
pub struct MonadCheckpoint {
    state: StateCheckpoint,
    ext: MonadEvmExt,
}

impl MonadCheckpoint {
    /// Synchronizes tracking and captures the current frame boundary.
    pub fn capture(host: &mut MonadHost<'_>) -> Result<Self, DatabaseError> {
        sync_reserve_balance(host)?;
        Ok(Self { state: host.state().checkpoint(), ext: host.ext().clone() })
    }

    /// Restores account, storage, log and Monad bookkeeping changes together.
    pub fn revert(self, host: &mut MonadHost<'_>) {
        let features = host.version().features;
        host.state_mut().rollback(self.state, features);
        *host.ext_mut() = self.ext;
    }
}

/// In-memory state and Monad context captured together for snapshot restoration.
///
/// The backing database and block/configuration belong to the embedder. Restore
/// those to the matching fork and execution point when restoring this snapshot.
#[derive(Clone, Debug)]
pub struct MonadStateSnapshot {
    state: StateSnapshot,
    ext: MonadEvmExt,
}

impl MonadStateSnapshot {
    /// Synchronizes reserves and captures native state, chain context and bookkeeping.
    pub fn capture(host: &mut MonadHost<'_>) -> Result<Self, DatabaseError> {
        sync_reserve_balance(host)?;
        Ok(Self { state: host.state().snapshot(), ext: host.ext().clone() })
    }

    /// Restores the snapshot over its matching backing database.
    pub fn restore<'a>(self, host: &mut MonadHost<'a>, db: impl DynDatabase + 'a) {
        *host.state_mut() = self.state.into_state(db);
        *host.ext_mut() = self.ext;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MonadBlockEnv, MonadEvmBuilder, MonadHardfork, page::is_page_cold};
    use alloy_primitives::{Address, U256};
    use evm2::evm::InMemoryDB;

    #[test]
    fn rollback_and_snapshot_restore_memory_pool_and_storage_pages_together() {
        let mut evm = MonadEvmBuilder::new(1).create_evm(
            MonadHardfork::MonadTen,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        let address = Address::repeat_byte(0x22);
        evm.ext_mut().memory_pool_offset = 64;
        let checkpoint = MonadCheckpoint::capture(&mut evm).unwrap();
        evm.ext_mut().memory_pool_offset = 128;
        evm.state_mut().storage_slot(&address, U256::ZERO).unwrap().warm();
        let snapshot = MonadStateSnapshot::capture(&mut evm).unwrap();
        checkpoint.revert(&mut evm);
        assert_eq!(evm.ext().memory_pool_offset(), 64);
        assert!(is_page_cold(&mut evm, address, U256::ZERO).unwrap());
        snapshot.restore(&mut evm, InMemoryDB::default());
        assert_eq!(evm.ext().memory_pool_offset(), 128);
        assert!(!is_page_cold(&mut evm, address, U256::ZERO).unwrap());
    }
}
