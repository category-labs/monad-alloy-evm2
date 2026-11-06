//! Block lifecycle helpers for Monad staking syscalls.
//!
//! ```text
//! ┌─ Block Start ─────────────────────────────────────────────┐
//! │ apply_syscall_reward(evm, block_author, block_reward)     │
//! ├─ Transactions ────────────────────────────────────────────┤
//! │ evm.transact(tx1)                                         │
//! │ ...                                                       │
//! ├─ Epoch Boundary (if applicable) ──────────────────────────┤
//! │ apply_epoch_boundary(evm, new_epoch)                      │
//! │   ├─ syscallSnapshot()                                    │
//! │   └─ syscallOnEpochChange(new_epoch)                      │
//! └───────────────────────────────────────────────────────────┘
//! ```
//!
//! In the Monad C++ client `syscallReward` is a system transaction with `msg.value` set to the
//! block reward. System calls carry no value, so [`syscall_reward_calldata`] appends the reward to
//! the calldata as a 32-byte extension, which the staking precompile reads. The caller is
//! responsible for minting the reward to [`STAKING_ADDRESS`] beforehand.
//!
//! These are native system calls, not signed transaction envelopes. They leave
//! sender nonces and fee balances unchanged. Returned gas measures execution
//! work and must not be added to ordinary transaction receipt or block gas.

use crate::{
    MonadEvmTypes,
    staking::{STAKING_ADDRESS, constants::SYSTEM_ADDRESS, interface::IMonadStaking::*},
};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;
use evm2::{
    Evm, ExecutedTx, TxResult,
    evm::{SYSTEM_CALL_GAS_LIMIT, SystemTx},
    interpreter::InstrStop,
    registry::{HandlerError, HandlerResult},
};

/// Encodes `syscallReward(address blockAuthor)` calldata with the block reward appended.
///
/// The standard ABI-encoded calldata (4 + 32 = 36 bytes) is extended with 32 bytes of big-endian
/// block reward.
pub fn syscall_reward_calldata(block_author: Address, block_reward: U256) -> Bytes {
    let mut data = syscallRewardCall { blockAuthor: block_author }.abi_encode();
    data.extend_from_slice(&block_reward.to_be_bytes::<32>());
    data.into()
}

/// Encodes `syscallSnapshot()` calldata.
pub fn syscall_snapshot_calldata() -> Bytes {
    syscallSnapshotCall {}.abi_encode().into()
}

/// Encodes `syscallOnEpochChange(uint64 epoch)` calldata.
pub fn syscall_on_epoch_change_calldata(new_epoch: u64) -> Bytes {
    syscallOnEpochChangeCall { epoch: new_epoch }.abi_encode().into()
}

/// Returns a staking system call from the Monad system sender with `data`.
///
/// Uses the 30M system-call gas budget, as revm system calls do.
pub fn staking_system_tx(data: Bytes) -> SystemTx {
    SystemTx::new(STAKING_ADDRESS, data)
        .with_caller(SYSTEM_ADDRESS)
        .with_gas_limit(SYSTEM_CALL_GAS_LIMIT)
}

/// Returns the `syscallReward` system call for a block author and reward.
pub fn syscall_reward_tx(block_author: Address, block_reward: U256) -> SystemTx {
    staking_system_tx(syscall_reward_calldata(block_author, block_reward))
}

/// Returns the `syscallSnapshot` system call.
pub fn syscall_snapshot_tx() -> SystemTx {
    staking_system_tx(syscall_snapshot_calldata())
}

/// Returns the `syscallOnEpochChange` system call.
pub fn syscall_on_epoch_change_tx(new_epoch: u64) -> SystemTx {
    staking_system_tx(syscall_on_epoch_change_calldata(new_epoch))
}

/// Applies the staking block reward via system call.
///
/// Should be called once per block. The caller must mint `block_reward` to [`STAKING_ADDRESS`]
/// beforehand. The returned handle must be resolved by the caller.
pub fn apply_syscall_reward<'e, 'a>(
    evm: &'e mut Evm<'a, MonadEvmTypes>,
    block_author: Address,
    block_reward: U256,
) -> HandlerResult<ExecutedTx<'e, 'a, MonadEvmTypes>> {
    evm.system_call(syscall_reward_tx(block_author, block_reward))
}

/// Applies the epoch snapshot via system call. The returned handle must be resolved by the caller.
pub fn apply_syscall_snapshot<'e, 'a>(
    evm: &'e mut Evm<'a, MonadEvmTypes>,
) -> HandlerResult<ExecutedTx<'e, 'a, MonadEvmTypes>> {
    evm.system_call(syscall_snapshot_tx())
}

/// Applies the epoch change via system call. The returned handle must be resolved by the caller.
pub fn apply_syscall_on_epoch_change<'e, 'a>(
    evm: &'e mut Evm<'a, MonadEvmTypes>,
    new_epoch: u64,
) -> HandlerResult<ExecutedTx<'e, 'a, MonadEvmTypes>> {
    evm.system_call(syscall_on_epoch_change_tx(new_epoch))
}

/// Applies the full epoch boundary transition: `syscallSnapshot()` then
/// `syscallOnEpochChange(new_epoch)`.
///
/// Both system calls are committed to the EVM's accepted state, in order.
/// Execution stops on the first unsuccessful call. This is not an atomic batch:
/// if the epoch change fails, the successful snapshot remains committed and the
/// caller can retry the epoch change or discard the enclosing block.
pub fn apply_epoch_boundary(
    evm: &mut Evm<'_, MonadEvmTypes>,
    new_epoch: u64,
) -> HandlerResult<(TxResult<MonadEvmTypes>, TxResult<MonadEvmTypes>)> {
    let snapshot = commit_successful_syscall(apply_syscall_snapshot(evm)?)?;
    let epoch_change = commit_successful_syscall(apply_syscall_on_epoch_change(evm, new_epoch)?)?;
    Ok((snapshot, epoch_change))
}

/// An unsuccessful staking operation in a block lifecycle helper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StakingSystemCallError {
    /// Interpreter stop reason.
    pub stop: InstrStop,
    /// Revert output.
    pub output: Bytes,
}

impl core::fmt::Display for StakingSystemCallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "staking system call failed: {:?}: {}", self.stop, self.output)
    }
}

impl core::error::Error for StakingSystemCallError {}

fn commit_successful_syscall(
    executed: ExecutedTx<'_, '_, MonadEvmTypes>,
) -> HandlerResult<TxResult<MonadEvmTypes>> {
    if executed.result().is_success() {
        Ok(executed.commit())
    } else {
        let result = executed.discard();
        Err(HandlerError::external(StakingSystemCallError {
            stop: result.stop,
            output: result.output,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calldata_matches_monad_revm() {
        let author = Address::new([0xAB; 20]);
        for reward in [U256::ZERO, U256::from(1_000_000_000_000_000_000u128), U256::MAX] {
            assert_eq!(
                syscall_reward_calldata(author, reward),
                monad_revm::api::block::syscall_reward_calldata(author, reward)
            );
        }
        assert_eq!(syscall_reward_calldata(author, U256::ZERO).len(), 68);
        assert_eq!(
            syscall_snapshot_calldata(),
            monad_revm::api::block::syscall_snapshot_calldata()
        );
        for epoch in [0, 42, u64::MAX] {
            assert_eq!(
                syscall_on_epoch_change_calldata(epoch),
                monad_revm::api::block::syscall_on_epoch_change_calldata(epoch)
            );
        }
        let tx = syscall_snapshot_tx();
        assert_eq!(tx.caller, monad_revm::staking::constants::SYSTEM_ADDRESS);
        assert_eq!(tx.system_contract_address, monad_revm::staking::STAKING_ADDRESS);
        assert_eq!(tx.gas_limit, 30_000_000);
    }
}
