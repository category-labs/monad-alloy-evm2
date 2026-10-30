//! Monad hardfork identifiers and activation schedules.
//!
//! The Ethereum spec returned by [`MonadHardfork::into_eth_spec`] selects the engine's
//! base rules. Each Monad revision adds its own rules and exceptions; it does not
//! enable every EIP in that Ethereum network upgrade.
//!
//! Across the supported revisions:
//! - EIP-2537 BLS12-381 precompiles, EIP-2935 block-hash history, EIP-7623
//!   calldata-floor validation, and EIP-7702 delegation are active. Monad adds
//!   delegation restrictions, including rejecting the system account as an authority
//!   and contract creation from delegated code.
//! - EIP-7951 P-256 verification is already active before MonadNine.
//! - Blob transactions (EIP-4844) are rejected, while `BLOBHASH`, `BLOBBASEFEE`, and
//!   the point-evaluation precompile remain available. Opcode availability and
//!   transaction-type support are separate rules.
//! - EIP-7685 execution-request processing and the EIP-7691/EIP-7918 blob changes
//!   are disabled. Ethereum's PeerDAS (EIP-7594) and proposer lookahead (EIP-7917)
//!   are not part of Monad's consensus protocol.
//! - Monad uses its own transaction and block limits instead of EIP-7825/EIP-7934,
//!   and charges the full transaction gas limit without Ethereum-style refunds.
//!
//! These protocol distinctions are enforced by configuration, instruction handlers,
//! precompiles, transaction validation, and block processing together.
//!
//! See [MIP-5] for the Fusaka selection and the [C++ revision traits] for the stable
//! Monad revision predicates. [`MonadHardfork::MonadNext`] retains the Rust release's
//! development rules and is not an alias for the C++ client's evolving `MONAD_NEXT`.
//!
//! [MIP-5]: https://github.com/monad-crypto/MIPs/blob/main/MIPs/MIP-5.md
//! [C++ revision traits]: https://github.com/category-labs/monad/blob/e8f793d145fad654b3a8b0b77c9826f0f2572312/category/vm/evm/traits.hpp

use core::{fmt, str::FromStr};
use evm2::SpecId;

/// Monad mainnet chain ID.
pub const MONAD_MAINNET_CHAIN_ID: u64 = 143;

/// Monad testnet chain ID.
pub const MONAD_TESTNET_CHAIN_ID: u64 = 10143;

/// Monad hardfork identifier, ordered by activation.
///
/// Each revision inherits earlier Monad rules. Discriminants follow the Rust API;
/// they are not the numeric values of the C++ client's `monad_revision` enum.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum MonadHardfork {
    /// Monad launch rules, using Prague as the Ethereum base.
    ///
    /// Includes the existing 128 KiB code and 256 KiB initcode limits documented in
    /// [MIP-2](https://github.com/monad-crypto/MIPs/blob/main/MIPs/MIP-2.md), Monad gas
    /// pricing, and the EIP selections described in this module.
    MonadEight = 100,
    /// MonadEight plus MIP-3, MIP-4, and MIP-5, using Osaka as the Ethereum base.
    ///
    /// [MIP-6](https://github.com/monad-crypto/MIPs/blob/main/MIPs/MIP-6.md) groups:
    /// - MIP-3: linear memory pricing and an 8 MiB pooled memory limit.
    /// - MIP-4: reserve-balance introspection at address `0x1001`.
    /// - MIP-5: EIP-7823 ModExp input limits, EIP-7883 ModExp repricing, and
    ///   EIP-7939's `CLZ` opcode. EIP-7951 is already active in MonadEight.
    MonadNine = 101,
    /// MonadNine plus [MIP-8](https://github.com/monad-crypto/MIPs/blob/main/MIPs/MIP-8.md).
    ///
    /// Adds page-based storage warming and SSTORE pricing, including page-aware
    /// EIP-2930 access lists. It retains the same Ethereum EIP selection as MonadNine.
    #[default]
    MonadTen = 102,
    /// Unscheduled development hardfork with monad-revm 0.8.0's MonadTen rules.
    ///
    /// The C++ client's `MONAD_NEXT` selects Amsterdam and additional development
    /// rules. Those rules are not implied by this release's `MonadNext` identifier.
    MonadNext = 103,
}

impl MonadHardfork {
    /// All supported hardforks in activation order.
    pub const ALL: [Self; 4] = [Self::MonadEight, Self::MonadNine, Self::MonadTen, Self::MonadNext];

    /// Returns the Ethereum foundation for this hardfork.
    ///
    /// Selects inherited opcode and gas tables; it does not certify support for the
    /// entire Ethereum fork. Monad precompiles, transaction validation, gas charging,
    /// and runtime limits apply the differences documented in this module.
    pub const fn into_eth_spec(self) -> SpecId {
        match self {
            Self::MonadEight => SpecId::PRAGUE,
            Self::MonadNine | Self::MonadTen | Self::MonadNext => SpecId::OSAKA,
        }
    }

    /// Returns whether this hardfork is active under `other`.
    pub const fn is_enabled_in(self, other: Self) -> bool {
        self as u8 <= other as u8
    }

    /// Returns the hardfork for a raw discriminant, or `None` for an unknown value.
    pub const fn try_from_u8(value: u8) -> Option<Self> {
        match value {
            100 => Some(Self::MonadEight),
            101 => Some(Self::MonadNine),
            102 => Some(Self::MonadTen),
            103 => Some(Self::MonadNext),
            _ => None,
        }
    }

    /// Returns the active hardfork for a chain ID and Unix timestamp in seconds.
    ///
    /// Unknown chains return `None`. Earlier revisions, including timestamps before
    /// MonadEight's activation, are represented as MonadEight. MonadNext has no
    /// scheduled activation and must be selected explicitly.
    pub const fn from_chain_and_timestamp(chain_id: u64, timestamp: u64) -> Option<Self> {
        let (nine, ten) = match chain_id {
            MONAD_MAINNET_CHAIN_ID => (
                Self::MonadNine.mainnet_activation_timestamp(),
                Self::MonadTen.mainnet_activation_timestamp(),
            ),
            MONAD_TESTNET_CHAIN_ID => (
                Self::MonadNine.testnet_activation_timestamp(),
                Self::MonadTen.testnet_activation_timestamp(),
            ),
            _ => return None,
        };

        if timestamp >= ten.unwrap() {
            Some(Self::MonadTen)
        } else if timestamp >= nine.unwrap() {
            Some(Self::MonadNine)
        } else {
            Some(Self::MonadEight)
        }
    }

    /// Returns the mainnet activation timestamp in seconds, if scheduled.
    pub const fn mainnet_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::MonadEight => Some(1_763_649_000),
            Self::MonadNine => Some(1_773_930_600),
            Self::MonadTen => Some(1_788_359_400),
            Self::MonadNext => None,
        }
    }

    /// Returns the testnet activation timestamp in seconds, if scheduled.
    pub const fn testnet_activation_timestamp(&self) -> Option<u64> {
        match self {
            Self::MonadEight => Some(1_763_562_600),
            Self::MonadNine => Some(1_773_153_000),
            Self::MonadTen => Some(1_786_545_000),
            Self::MonadNext => None,
        }
    }
}

impl From<MonadHardfork> for SpecId {
    fn from(spec: MonadHardfork) -> Self {
        spec.into_eth_spec()
    }
}

impl FromStr for MonadHardfork {
    type Err = UnknownMonadHardfork;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            s if s.eq_ignore_ascii_case(name::MONAD_EIGHT) => Ok(Self::MonadEight),
            s if s.eq_ignore_ascii_case(name::MONAD_NINE) => Ok(Self::MonadNine),
            s if s.eq_ignore_ascii_case(name::MONAD_TEN) => Ok(Self::MonadTen),
            s if s.eq_ignore_ascii_case(name::MONAD_NEXT) => Ok(Self::MonadNext),
            _ => Err(UnknownMonadHardfork),
        }
    }
}

impl From<MonadHardfork> for &'static str {
    fn from(spec: MonadHardfork) -> Self {
        match spec {
            MonadHardfork::MonadEight => name::MONAD_EIGHT,
            MonadHardfork::MonadNine => name::MONAD_NINE,
            MonadHardfork::MonadTen => name::MONAD_TEN,
            MonadHardfork::MonadNext => name::MONAD_NEXT,
        }
    }
}

impl fmt::Display for MonadHardfork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str((*self).into())
    }
}

/// Error returned when parsing an unknown Monad hardfork name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnknownMonadHardfork;

impl fmt::Display for UnknownMonadHardfork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unknown Monad hardfork")
    }
}

impl core::error::Error for UnknownMonadHardfork {}

pub mod name {
    //! Canonical string identifiers for Monad hardforks.

    /// MonadEight's name.
    pub const MONAD_EIGHT: &str = "MonadEight";
    /// MonadNine's name.
    pub const MONAD_NINE: &str = "MonadNine";
    /// MonadTen's name.
    pub const MONAD_TEN: &str = "MonadTen";
    /// MonadNext's name.
    pub const MONAD_NEXT: &str = "MonadNext";
}
