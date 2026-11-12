//! [MIP-8] storage pages and transaction-local gas accounting.
//!
//! A page contains 128 consecutive storage slots. A page is warm whenever any
//! slot in it is warm in evm2's state, including access-list warmth. Write flags
//! and growth counters live in evm2's transient journal, so frame rollback and
//! transaction cleanup restore them together with storage. The tracker uses ecRecover's
//! transient namespace: this permanent precompile never executes contract code
//! or uses transient storage. Contract TLOAD/TSTORE therefore cannot reach it.
//!
//! [MIP-8]: https://mips.monad.xyz/MIPs/MIP-8

use crate::MonadHost;
use alloy_primitives::{Address, U256, keccak256};
use evm2::DatabaseError;

/// Number of low storage-key bits used as an offset within a page.
pub const PAGE_SHIFT: usize = 7;
/// Number of 32-byte storage words in a 4 KiB page.
pub const WORDS_PER_PAGE: u64 = 1 << PAGE_SHIFT;
/// Base gas charged for every storage read or write.
pub const BASE_COST: u64 = 100;
/// Additional gas for the first value-changing write to a page.
pub const PAGE_WRITE_COST: u64 = 2_800;
/// Gas for increasing a page's peak net growth by one nonzero slot.
pub const STATE_GROWTH_COST: u64 = 17_000;

const TRACKER_ADDRESS: Address = Address::with_last_byte(1);

/// Returns the page index containing `slot`.
#[inline]
pub fn page_index(slot: U256) -> U256 {
    slot >> PAGE_SHIFT
}

/// Returns the word offset of `slot` within its page.
#[inline]
pub fn page_offset(slot: U256) -> U256 {
    slot & U256::from(WORDS_PER_PAGE - 1)
}

/// Queries native warmth without loading any storage values or maintaining another access flag.
pub(crate) fn is_page_cold(
    host: &mut MonadHost<'_>,
    address: Address,
    slot: U256,
) -> Result<bool, DatabaseError> {
    let storage = host.state_mut().storage(&address)?;
    if storage.is_warm(&slot) {
        return Ok(false);
    }
    let first = page_index(slot) << PAGE_SHIFT;
    Ok(!(0..WORDS_PER_PAGE).any(|offset| storage.is_warm(&(first + U256::from(offset)))))
}

/// Identifies a page in an account's storage, independently of its code address.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct StoragePageKey {
    /// Account owning the storage.
    pub address: Address,
    /// Page index, obtained by shifting a storage key right by seven bits.
    pub page: U256,
}

impl StoragePageKey {
    /// Creates a page key from its owning account and a storage slot.
    pub fn from_slot(address: Address, slot: U256) -> Self {
        Self { address, page: page_index(slot) }
    }

    fn tracker_slot(self) -> U256 {
        let mut input = [0; 57];
        input[..5].copy_from_slice(b"MIP-8");
        input[5..25].copy_from_slice(self.address.as_slice());
        input[25..].copy_from_slice(&self.page.to_be_bytes::<32>());
        keccak256(input).into()
    }
}

/// One page's bookkeeping, packed into a single journaled transient word.
pub(crate) struct PageAccess {
    slot: U256,
    written: bool,
    current_growth: i16,
    peak_growth: i16,
}

impl PageAccess {
    pub(crate) fn load(evm: &mut MonadHost<'_>, key: StoragePageKey) -> Self {
        let slot = key.tracker_slot();
        let encoded = evm.state_mut().tload(&TRACKER_ADDRESS, &slot).as_limbs()[0];
        Self {
            slot,
            written: encoded & 2 != 0,
            current_growth: (encoded >> 16) as u16 as i16,
            peak_growth: (encoded >> 32) as u16 as i16,
        }
    }

    pub(crate) fn record_write(
        &mut self,
        evm: &mut MonadHost<'_>,
        present: U256,
        new: U256,
    ) -> u64 {
        let mut gas = BASE_COST;
        if present != new && !self.written {
            self.written = true;
            gas += PAGE_WRITE_COST;
        }
        // A page has only 128 slots, so both signed counters fit in i16.
        self.current_growth += i16::from(present.is_zero()) - i16::from(new.is_zero());
        if self.current_growth > self.peak_growth {
            self.peak_growth = self.current_growth;
            gas += STATE_GROWTH_COST;
        }
        self.store(evm);
        gas
    }

    fn store(&self, evm: &mut MonadHost<'_>) {
        let encoded = (u64::from(self.written) << 1)
            | (u64::from(self.current_growth as u16) << 16)
            | (u64::from(self.peak_growth as u16) << 32);
        evm.state_mut().tstore(&TRACKER_ADDRESS, &self.slot, &U256::from(encoded));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MonadBlockEnv, MonadEvmBuilder, MonadHardfork, reserve_balance::MonadCheckpoint};
    use evm2::evm::InMemoryDB;

    #[test]
    fn native_warmth_is_scoped_to_the_page_and_storage_account() {
        let address = Address::repeat_byte(0x22);
        for prewarmed in [false, true] {
            let mut evm = MonadEvmBuilder::new(1).create_evm(
                MonadHardfork::MonadTen,
                MonadBlockEnv::default(),
                InMemoryDB::default(),
            );
            if prewarmed {
                evm.state_mut().prewarm_storage_slot(&address, U256::ZERO);
            } else {
                evm.state_mut().storage_slot(&address, U256::ZERO).unwrap().warm();
            }
            assert!(!is_page_cold(&mut evm, address, U256::from(127)).unwrap());
            assert!(is_page_cold(&mut evm, address, U256::from(128)).unwrap());
            assert!(is_page_cold(&mut evm, Address::repeat_byte(0x33), U256::ZERO).unwrap());
            // Page queries consult warmth only, including when a nested host has no tracker.
            assert!(!evm.state_mut().storage(&address).unwrap().is_loaded(&U256::from(127)));
            assert!(!evm.state_mut().storage(&address).unwrap().is_loaded(&U256::from(128)));
        }
    }

    #[test]
    fn native_warmth_and_write_growth_revert_with_the_frame() {
        let address = Address::repeat_byte(0x22);
        let mut evm = MonadEvmBuilder::new(1).create_evm(
            MonadHardfork::MonadTen,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        let key = StoragePageKey::from_slot(address, U256::ZERO);
        let cold = MonadCheckpoint::capture(&mut evm).unwrap();
        evm.state_mut().storage_slot(&address, U256::ZERO).unwrap().warm();
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ZERO, U256::ONE),
            BASE_COST + PAGE_WRITE_COST + STATE_GROWTH_COST
        );
        let warm = MonadCheckpoint::capture(&mut evm).unwrap();
        evm.state_mut().storage_slot(&address, U256::from(128)).unwrap().warm();
        assert!(!is_page_cold(&mut evm, address, U256::from(128)).unwrap());
        warm.revert(&mut evm);
        assert!(is_page_cold(&mut evm, address, U256::from(128)).unwrap());
        assert!(!is_page_cold(&mut evm, address, U256::ZERO).unwrap());

        evm.state_mut().set_storage_warm(&address, U256::ZERO, false);
        evm.state_mut().set_storage_warm(&address, U256::ZERO, true);
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ONE, U256::ZERO),
            BASE_COST
        );
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ZERO, U256::ONE),
            BASE_COST
        );
        cold.revert(&mut evm);
        assert!(is_page_cold(&mut evm, address, U256::ZERO).unwrap());
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ZERO, U256::ONE),
            BASE_COST + PAGE_WRITE_COST + STATE_GROWTH_COST
        );
    }

    #[test]
    fn cooling_preserves_other_warm_slots_and_access_list_warmth() {
        let address = Address::repeat_byte(0x22);
        let mut evm = MonadEvmBuilder::new(1).create_evm(
            MonadHardfork::MonadTen,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        for slot in [0, 127] {
            evm.state_mut().storage_slot(&address, U256::from(slot)).unwrap().warm();
        }
        evm.state_mut().set_storage_warm(&address, U256::ZERO, false);
        assert!(!is_page_cold(&mut evm, address, U256::ZERO).unwrap());
        evm.state_mut().set_storage_warm(&address, U256::from(127), false);
        assert!(is_page_cold(&mut evm, address, U256::ZERO).unwrap());
        evm.state_mut().prewarm_storage_slot(&address, U256::from(127));
        evm.state_mut().set_storage_warm(&address, U256::from(127), false);
        assert!(!is_page_cold(&mut evm, address, U256::ZERO).unwrap());
        assert!(is_page_cold(&mut evm, address, U256::from(128)).unwrap());
    }
}
