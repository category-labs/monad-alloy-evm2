//! Replay of trusted Monad protocol transaction envelopes.
//!
//! Enable [`MonadEvmExt::protocol_replay`](crate::MonadEvmExt::protocol_replay) when
//! replaying historical blocks. Only the reserved system sender is routed here.
//! Its legacy envelope must call a canonical staking syscall with zero gas limit
//! and price. Replay checks the chain ID and nonce, increments the nonce, mints
//! any block reward, and executes an inspected system call with zero receipt gas.
//! A failed syscall rejects the transaction, including the nonce and mint.
//!
//! The caller must authenticate the reserved sender before constructing a
//! recovered envelope. This module does not recover protocol signatures, validate
//! block placement or epoch scheduling, or verify the prescribed reward amount.
//! Keep replay disabled for transaction-pool admission and ordinary execution.

use crate::{
    MonadEvmTypes, MonadHost, MonadTxEnvelope, SYSTEM_ADDRESS,
    staking::{STAKING_ADDRESS, interface::IMonadStaking::*},
    staking_system_tx, syscall_on_epoch_change_calldata, syscall_reward_calldata,
    syscall_snapshot_calldata,
};
use alloy_consensus::{Typed2718, transaction::Recovered};
use alloy_primitives::{Address, Bytes, TxKind, U256};
use alloy_sol_types::SolCall;
use core::{fmt, sync::atomic::Ordering};
use evm2::{
    TxResult,
    evm::SystemTx,
    interpreter::InstrStop,
    registry::{HandlerError, HandlerResult},
};

/// A decoded canonical protocol envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProtocolSystemCall {
    /// Reserved protocol caller.
    pub caller: Address,
    /// Staking contract address.
    pub contract: Address,
    /// Syscall input, including the reward extension when applicable.
    pub data: Bytes,
    /// System sender nonce from the envelope.
    pub nonce: u64,
    /// Optional EIP-155 chain ID from the envelope.
    pub chain_id: Option<u64>,
    /// Reward recipient and amount to mint before execution.
    pub balance_increment: Option<(Address, U256)>,
}

impl ProtocolSystemCall {
    /// Returns the native syscall. This alone does not apply its nonce or mint.
    pub fn system_tx(&self) -> SystemTx {
        staking_system_tx(self.data.clone())
    }

    /// Checks the envelope's optional chain ID against the active chain.
    pub const fn validate_chain_id(&self, chain_id: u64) -> Result<(), ProtocolSystemCallError> {
        if let Some(got) = self.chain_id
            && got != chain_id
        {
            return Err(ProtocolSystemCallError::ChainIdMismatch { expected: chain_id, got });
        }
        Ok(())
    }

    pub(crate) fn prepare(&self, host: &mut MonadHost<'_>) -> HandlerResult<()> {
        self.validate_chain_id(host.version().chain_id)?;
        let next_nonce = self.nonce.checked_add(1).ok_or(ProtocolSystemCallError::NonceOverflow)?;
        let nonce = host.state_mut().account(&self.caller)?.nonce();
        if nonce != self.nonce {
            return Err(ProtocolSystemCallError::NonceMismatch {
                expected: nonce,
                got: self.nonce,
            }
            .into());
        }
        let mint = if let Some((address, amount)) = self.balance_increment {
            let balance = host
                .state_mut()
                .account(&address)?
                .balance()
                .checked_add(amount)
                .ok_or(ProtocolSystemCallError::BalanceOverflow)?;
            Some((address, balance))
        } else {
            None
        };
        host.state_mut().account(&self.caller)?.set_nonce(next_nonce);
        if let Some((address, balance)) = mint {
            host.state_mut().account(&address)?.set_balance(balance);
        }
        Ok(())
    }

    pub(crate) fn execute(
        self,
        host: &mut MonadHost<'_>,
    ) -> HandlerResult<TxResult<MonadEvmTypes>> {
        // Require the wrapper's isolated context and scoped inspector lifecycle.
        let dispatched = host
            .ext()
            .protocol_dispatch
            .as_ref()
            .ok_or_else(|| HandlerError::external(crate::transaction::MissingMonadExecutor))?;
        dispatched.store(true, Ordering::Relaxed);
        let mut result = host.execute_system_call(self.system_tx())?;
        if !result.is_success() {
            return Err(ProtocolSystemCallError::ExecutionFailed {
                stop: result.stop,
                output: result.output,
            }
            .into());
        }
        result.total_gas_spent = 0;
        result.state_gas_spent = 0;
        result.refunded = 0;
        result.floor_gas = 0;
        Ok(result)
    }
}

/// Invalid protocol envelope or unsuccessful protocol execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtocolSystemCallError {
    /// Only legacy envelopes are supported.
    NotLegacy {
        /// Received EIP-2718 type.
        tx_type: u8,
    },
    /// Destination must be the staking contract.
    InvalidTarget(TxKind),
    /// Protocol envelopes must have zero gas limit.
    NonZeroGasLimit(u64),
    /// Protocol envelopes must have zero gas price.
    NonZeroGasPrice(u128),
    /// Input is shorter than a four-byte selector.
    MissingSelector,
    /// Selector does not identify a protocol syscall.
    UnknownSelector([u8; 4]),
    /// Input does not have the syscall's exact length.
    InvalidCalldataLength {
        /// Syscall selector.
        selector: [u8; 4],
        /// Required number of bytes.
        expected: usize,
        /// Received number of bytes.
        got: usize,
    },
    /// Input contains noncanonical ABI words.
    NonCanonicalCalldata {
        /// Syscall selector.
        selector: [u8; 4],
    },
    /// Only the reward syscall may carry value.
    NonZeroValue {
        /// Syscall selector.
        selector: [u8; 4],
    },
    /// Envelope chain ID differs from the configured chain.
    ChainIdMismatch {
        /// Configured chain ID.
        expected: u64,
        /// Envelope chain ID.
        got: u64,
    },
    /// Envelope nonce differs from state.
    NonceMismatch {
        /// State nonce.
        expected: u64,
        /// Envelope nonce.
        got: u64,
    },
    /// Sender nonce cannot be incremented.
    NonceOverflow,
    /// Mint would overflow the staking balance.
    BalanceOverflow,
    /// Reverted or halted syscall; all transaction writes are discarded.
    ExecutionFailed {
        /// Execution stop reason.
        stop: InstrStop,
        /// Failure output.
        output: Bytes,
    },
}

impl fmt::Display for ProtocolSystemCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid Monad protocol transaction: {self:?}")
    }
}

impl core::error::Error for ProtocolSystemCallError {}

impl From<ProtocolSystemCallError> for HandlerError {
    fn from(error: ProtocolSystemCallError) -> Self {
        Self::external(error)
    }
}

/// Decodes a protocol envelope, trusting its already recovered signer.
///
/// Returns `Ok(None)` for other senders, and rejects malformed envelopes from
/// the system sender. State-dependent chain ID, nonce and mint checks run during
/// transaction admission. See the [module documentation](self) for caller duties.
pub fn protocol_system_call(
    tx: &Recovered<MonadTxEnvelope>,
) -> Result<Option<ProtocolSystemCall>, ProtocolSystemCallError> {
    decode(tx.signer(), tx.inner())
}

pub(crate) fn decode(
    caller: Address,
    envelope: &MonadTxEnvelope,
) -> Result<Option<ProtocolSystemCall>, ProtocolSystemCallError> {
    if caller != SYSTEM_ADDRESS {
        return Ok(None);
    }
    let tx = envelope
        .as_legacy()
        .ok_or(ProtocolSystemCallError::NotLegacy { tx_type: envelope.ty() })?;
    if tx.to != TxKind::Call(STAKING_ADDRESS) {
        return Err(ProtocolSystemCallError::InvalidTarget(tx.to));
    }
    if tx.gas_limit != 0 {
        return Err(ProtocolSystemCallError::NonZeroGasLimit(tx.gas_limit));
    }
    if tx.gas_price != 0 {
        return Err(ProtocolSystemCallError::NonZeroGasPrice(tx.gas_price));
    }
    let selector = tx
        .input
        .get(..4)
        .ok_or(ProtocolSystemCallError::MissingSelector)?
        .try_into()
        .expect("four selector bytes");
    let expected = match selector {
        syscallRewardCall::SELECTOR | syscallOnEpochChangeCall::SELECTOR => 36,
        syscallSnapshotCall::SELECTOR => 4,
        _ => return Err(ProtocolSystemCallError::UnknownSelector(selector)),
    };
    if tx.input.len() != expected {
        return Err(ProtocolSystemCallError::InvalidCalldataLength {
            selector,
            expected,
            got: tx.input.len(),
        });
    }
    if selector != syscallRewardCall::SELECTOR && !tx.value.is_zero() {
        return Err(ProtocolSystemCallError::NonZeroValue { selector });
    }
    let malformed = || ProtocolSystemCallError::NonCanonicalCalldata { selector };
    let (data, balance_increment) = match selector {
        syscallRewardCall::SELECTOR => {
            let call =
                syscallRewardCall::abi_decode_raw(&tx.input[4..]).map_err(|_| malformed())?;
            if call.abi_encode() != tx.input.as_ref() {
                return Err(malformed());
            }
            (syscall_reward_calldata(call.blockAuthor, tx.value), Some((STAKING_ADDRESS, tx.value)))
        }
        syscallOnEpochChangeCall::SELECTOR => {
            let call = syscallOnEpochChangeCall::abi_decode_raw(&tx.input[4..])
                .map_err(|_| malformed())?;
            if call.abi_encode() != tx.input.as_ref() {
                return Err(malformed());
            }
            (syscall_on_epoch_change_calldata(call.epoch), None)
        }
        _ => (syscall_snapshot_calldata(), None),
    };
    Ok(Some(ProtocolSystemCall {
        caller,
        contract: STAKING_ADDRESS,
        data,
        nonce: tx.nonce,
        chain_id: tx.chain_id,
        balance_increment,
    }))
}
