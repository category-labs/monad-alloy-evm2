//! Comparable execution observations and explicit reference differences.

use alloy_primitives::{Address, B256, Bytes, Log};
use serde::Serialize;

/// A transaction or system call's observable result, including its complete post-state root.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Observation {
    /// Success, revert, exceptional halt, or Monad reserve rejection.
    pub status: &'static str,
    /// Execution gas consumed before refunds and the receipt floor.
    pub gas_spent: u64,
    /// Raw refund counter after settlement.
    pub refunded: u64,
    /// Charged receipt gas; system calls do not have receipts.
    pub receipt_gas: Option<u64>,
    /// Return or revert bytes.
    pub output: Bytes,
    /// Emitted logs in execution order.
    pub logs: Vec<Log>,
    /// Address returned by a successful creation.
    pub created_address: Option<Address>,
    /// Commitment to all accounts, code, balances, nonces and persistent storage.
    pub state_root: B256,
}

/// A disagreement with the published monad-revm 0.8.0 reference.
#[derive(Debug, Serialize)]
pub struct ParityDifference {
    /// Block index and transaction index or system-call address.
    pub at: String,
    /// The adapter's observation.
    pub actual: Observation,
    /// The reference's observation, when execution completed.
    pub reference: Option<Observation>,
    /// Reference execution error, when no observation could be produced.
    pub reference_error: Option<String>,
}

/// Reference checks completed before fixture verification stopped.
#[derive(Debug, Default, Serialize)]
pub struct ParityReport {
    /// Number of transaction and system-call comparisons.
    pub checks: usize,
    /// Reference receipts normalized from consumed gas to the full gas limit.
    pub corrected_receipts: usize,
    /// Every observed difference; the runner has no implicit exemptions.
    pub differences: Vec<ParityDifference>,
}

impl ParityReport {
    pub(crate) fn record(
        &mut self,
        at: String,
        actual: Observation,
        reference: eyre::Result<(Observation, bool)>,
    ) {
        self.checks += 1;
        match reference {
            Ok((reference, corrected)) => {
                self.corrected_receipts += usize::from(corrected);
                if actual != reference {
                    self.differences.push(ParityDifference {
                        at,
                        actual,
                        reference: Some(reference),
                        reference_error: None,
                    });
                }
            }
            Err(error) => self.differences.push(ParityDifference {
                at,
                actual,
                reference: None,
                reference_error: Some(format!("{error:#}")),
            }),
        }
    }
}
