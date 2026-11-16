use alloy_primitives::{Address, B256, U256};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, CacheDB, DbResult, DynDatabase},
};

pub(super) type LocalDB = CacheDB<EmptyLocalDB>;

/// Empty state without synthetic block history; supplied values live in the cache.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct EmptyLocalDB;

impl DynDatabase for EmptyLocalDB {
    fn get_account(&mut self, _: &Address) -> DbResult<Option<AccountInfo>> {
        Ok(None)
    }
    fn get_code_by_hash(&mut self, _: &B256) -> DbResult<Bytecode> {
        Ok(Bytecode::default())
    }
    fn get_storage(&mut self, _: &Address, _: &U256) -> DbResult<U256> {
        Ok(U256::ZERO)
    }
    fn get_block_hash(&mut self, _: &U256) -> DbResult<B256> {
        Ok(B256::ZERO)
    }
}
