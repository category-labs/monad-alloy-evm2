//! [MIP-8] storage pages and transaction-local gas accounting.
//!
//! A page contains 128 consecutive storage slots. Its access flags and growth
//! counters live in evm2's transient journal, so frame rollback and transaction
//! cleanup restore them together with storage. The tracker uses ecRecover's
//! transient namespace: this permanent precompile never executes contract code
//! or uses transient storage. Contract TLOAD/TSTORE therefore cannot reach it.
//!
//! [MIP-8]: https://mips.monad.xyz/MIPs/MIP-8

use crate::{MonadHardfork, MonadHost, frame_hardfork};
use alloy_primitives::{Address, U256, keccak256};

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
    accessed: bool,
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
            accessed: encoded & 1 != 0,
            written: encoded & 2 != 0,
            current_growth: (encoded >> 16) as u16 as i16,
            peak_growth: (encoded >> 32) as u16 as i16,
        }
    }

    pub(crate) const fn is_cold(&self) -> bool {
        !self.accessed
    }

    pub(crate) fn record_read(&mut self, evm: &mut MonadHost<'_>) {
        self.accessed = true;
        self.store(evm);
    }

    pub(crate) fn record_write(
        &mut self,
        evm: &mut MonadHost<'_>,
        present: U256,
        new: U256,
    ) -> u64 {
        let mut gas = BASE_COST;
        self.accessed = true;
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
        let encoded = u64::from(self.accessed)
            | (u64::from(self.written) << 1)
            | (u64::from(self.current_growth as u16) << 16)
            | (u64::from(self.peak_growth as u16) << 32);
        evm.state_mut().tstore(&TRACKER_ADDRESS, &self.slot, &U256::from(encoded));
    }
}

/// Sets the warmth of the page containing `slot` from Monad Ten onward.
///
/// Use this after an inspector changes storage warmth outside SLOAD/SSTORE. It
/// does not load or change the slot, charge gas, or reset the page's write and
/// growth accounting. Transaction-prewarmed pages stay warm. The change is
/// journaled and reverts with its frame.
pub fn set_storage_page_warm(host: &mut MonadHost<'_>, address: Address, slot: U256, warm: bool) {
    if frame_hardfork(host) < MonadHardfork::MonadTen {
        return;
    }
    let key = StoragePageKey::from_slot(address, slot);
    let mut page = PageAccess::load(host, key);
    page.accessed = warm
        || (0..WORDS_PER_PAGE).any(|offset| {
            let slot = (key.page << PAGE_SHIFT) + U256::from(offset);
            host.state().prewarm_set().is_storage_warm(&address, &slot)
        });
    page.store(host);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MonadBlockEnv, MonadEvmBuilder, reserve_balance::MonadCheckpoint};
    use evm2::evm::InMemoryDB;

    #[test]
    fn inspector_warmth_is_scoped_to_the_page_and_active_fork() {
        let address = Address::repeat_byte(0x22);
        for fork in [MonadHardfork::MonadNine, MonadHardfork::MonadTen] {
            let mut evm = MonadEvmBuilder::new(1).create_evm(
                fork,
                MonadBlockEnv::default(),
                InMemoryDB::default(),
            );
            set_storage_page_warm(&mut evm, address, U256::ZERO, true);
            let same = StoragePageKey::from_slot(address, U256::from(127));
            let next = StoragePageKey::from_slot(address, U256::from(128));
            let other = StoragePageKey::from_slot(Address::repeat_byte(0x33), U256::ZERO);
            assert_eq!(PageAccess::load(&mut evm, same).is_cold(), fork < MonadHardfork::MonadTen);
            assert!(PageAccess::load(&mut evm, next).is_cold());
            assert!(PageAccess::load(&mut evm, other).is_cold());
        }
    }

    #[test]
    fn inspector_warmth_preserves_write_charges_and_reverts_with_the_frame() {
        let address = Address::repeat_byte(0x22);
        let mut evm = MonadEvmBuilder::new(1).create_evm(
            MonadHardfork::MonadTen,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        let key = StoragePageKey::from_slot(address, U256::ZERO);
        let cold = MonadCheckpoint::capture(&mut evm).unwrap();
        set_storage_page_warm(&mut evm, address, U256::ZERO, true);
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ZERO, U256::ONE),
            BASE_COST + PAGE_WRITE_COST + STATE_GROWTH_COST
        );
        let warm = MonadCheckpoint::capture(&mut evm).unwrap();
        set_storage_page_warm(&mut evm, address, U256::from(127), false);
        assert!(PageAccess::load(&mut evm, key).is_cold());
        warm.revert(&mut evm);
        assert!(!PageAccess::load(&mut evm, key).is_cold());

        set_storage_page_warm(&mut evm, address, U256::ZERO, false);
        set_storage_page_warm(&mut evm, address, U256::ZERO, true);
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ONE, U256::ZERO),
            BASE_COST
        );
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ZERO, U256::ONE),
            BASE_COST
        );
        cold.revert(&mut evm);
        assert!(PageAccess::load(&mut evm, key).is_cold());
        assert_eq!(
            PageAccess::load(&mut evm, key).record_write(&mut evm, U256::ZERO, U256::ONE),
            BASE_COST + PAGE_WRITE_COST + STATE_GROWTH_COST
        );
    }

    #[test]
    fn cooling_preserves_transaction_prewarmed_pages() {
        let address = Address::repeat_byte(0x22);
        let mut evm = MonadEvmBuilder::new(1).create_evm(
            MonadHardfork::MonadTen,
            MonadBlockEnv::default(),
            InMemoryDB::default(),
        );
        evm.state_mut().prewarm_storage_slot(&address, U256::from(127));
        set_storage_page_warm(&mut evm, address, U256::ZERO, false);
        let page = StoragePageKey::from_slot(address, U256::ZERO);
        assert!(!PageAccess::load(&mut evm, page).is_cold());
        let next = StoragePageKey::from_slot(address, U256::from(128));
        set_storage_page_warm(&mut evm, address, U256::from(128), true);
        set_storage_page_warm(&mut evm, address, U256::from(128), false);
        assert!(PageAccess::load(&mut evm, next).is_cold());
    }
}
