//! Exact, documented differences between fixture expectations and the execution client.

use crate::{CaseReport, Status};
use serde::Serialize;

const HIGH_GAS_USAGE_CASE: &str = "./updated_2/highGasUsage.json::highGasUsage_MONAD_EIGHT";
const HIGH_GAS_USAGE_FAILURE: &str = concat!(
    "block 1: receipts root mismatch: expected ",
    "0x0ea87bd79fe41df810ea2d14ab13c6a3095c5ac9f181a747d9eccca4df09a392, got ",
    "0x71ddce3aeced8691b6b4a222fdf0249f8016197a484b54fc5915793fda7b67eb; ",
    "transactions [0: reserve_balance_violation]"
);

/// A known specification-model difference; the underlying fixture still fails.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KnownSpecModelDivergence {
    /// The model permits a non-emptying transaction whose gas fee exceeds its reserve.
    GasFeesExceedReserve,
}

impl KnownSpecModelDivergence {
    /// Explains why the fixture expectation differs from execution-client behavior.
    pub const fn description(self) -> &'static str {
        match self {
            Self::GasFeesExceedReserve => {
                "highGasUsage block 2 charges 10.2 MON against a 10 MON reserve without the \
                 emptying exception; the OCaml model clamps the threshold to zero, while \
                 the execution client rejects the transaction"
            }
        }
    }

    pub(crate) fn classify(case: &CaseReport) -> Option<Self> {
        // A name or reserve violation alone must never exempt a different failure.
        (case.file.ends_with("valid_blocks/highGasUsage.json")
            && case.name == HIGH_GAS_USAGE_CASE
            && case.network.as_deref() == Some("MONAD_EIGHT")
            && case.status == Status::Failed
            && case.message == HIGH_GAS_USAGE_FAILURE)
            .then_some(Self::GasFeesExceedReserve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn high_gas_usage_failure() -> CaseReport {
        CaseReport {
            file: "/fixtures/valid_blocks/highGasUsage.json".into(),
            name: HIGH_GAS_USAGE_CASE.into(),
            network: Some("MONAD_EIGHT".into()),
            status: Status::Failed,
            message: HIGH_GAS_USAGE_FAILURE.into(),
            known_spec_model_divergence: None,
            parity: Default::default(),
        }
    }

    #[test]
    fn classifies_the_exact_high_gas_usage_failure() {
        let classification = KnownSpecModelDivergence::classify(&high_gas_usage_failure());
        assert_eq!(classification, Some(KnownSpecModelDivergence::GasFeesExceedReserve));
        assert_eq!(serde_json::to_value(classification).unwrap(), "gas_fees_exceed_reserve");
    }

    #[test]
    fn leaves_other_cases_and_changed_outcomes_unclassified() {
        let changes: [fn(&mut CaseReport); 10] = [
            |case| case.file = "/fixtures/valid_blocks/other.json".into(),
            |case| case.file = "/fixtures/invalid_blocks/highGasUsage.json".into(),
            |case| case.name = "other::highGasUsage_MONAD_EIGHT".into(),
            |case| case.network = Some("MONAD_NINE".into()),
            |case| case.network = None,
            |case| case.status = Status::Passed,
            |case| case.status = Status::Unsupported,
            |case| case.message = case.message.replace("block 1", "block 0"),
            |case| case.message = case.message.replace("0x0ea87", "0x0ea88"),
            |case| case.message = case.message.replace("reserve_balance_violation", "halt"),
        ];
        for change in changes {
            let mut case = high_gas_usage_failure();
            change(&mut case);
            assert_eq!(KnownSpecModelDivergence::classify(&case), None, "{case:?}");
        }
    }
}
