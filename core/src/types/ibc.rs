//! IBC-related data types

use std::cmp::Ordering;
use std::collections::HashMap;

use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};

/// Wrapped IbcEvent
#[derive(
    Debug, Clone, BorshSerialize, BorshDeserialize, BorshSchema, PartialEq, Eq,
)]
pub struct IbcEvent {
    /// The IBC event type
    pub event_type: String,
    /// The attributes of the IBC event
    pub attributes: HashMap<String, String>,
}

impl std::cmp::PartialOrd for IbcEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.event_type.partial_cmp(&other.event_type)
    }
}

impl std::cmp::Ord for IbcEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        // should not compare the same event type
        self.event_type.cmp(&other.event_type)
    }
}

impl std::fmt::Display for IbcEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let attributes = self
            .attributes
            .iter()
            .map(|(k, v)| format!("{}: {};", k, v))
            .collect::<Vec<String>>()
            .join(", ");
        write!(
            f,
            "Event type: {}, Attributes: {}",
            self.event_type, attributes
        )
    }
}

/// IBC shielded transfer
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct IbcShieldedTransfer {
    /// The IBC event type
    pub transfer: crate::types::token::Transfer,
    /// The attributes of the IBC event
    pub masp_tx: masp_primitives::transaction::Transaction,
}

#[cfg(any(feature = "abciplus", feature = "abcipp"))]
mod ibc_rs_conversion {
    use std::collections::HashMap;
    use std::str::FromStr;

    use borsh::{BorshDeserialize, BorshSerialize};
    use data_encoding::HEXLOWER;
    use thiserror::Error;

    use super::{IbcEvent, IbcShieldedTransfer};
    use crate::ibc::applications::transfer::Memo;
    use crate::ibc::core::events::{
        Error as IbcEventError, IbcEvent as RawIbcEvent,
    };
    use crate::tendermint_proto::abci::Event as AbciEvent;
    use crate::types::masp::PaymentAddress;

    #[allow(missing_docs)]
    #[derive(Error, Debug)]
    pub enum Error {
        #[error("IBC event error: {0}")]
        IbcEvent(IbcEventError),
        #[error("IBC transfer memo HEX decoding error: {0}")]
        DecodingHex(data_encoding::DecodeError),
        #[error("IBC transfer memo decoding error: {0}")]
        DecodingShieldedTransfer(std::io::Error),
    }

    /// Conversion functions result
    pub type Result<T> = std::result::Result<T, Error>;

    impl TryFrom<RawIbcEvent> for IbcEvent {
        type Error = Error;

        fn try_from(e: RawIbcEvent) -> Result<Self> {
            let event_type = e.event_type().to_string();
            let abci_event = AbciEvent::try_from(e).map_err(Error::IbcEvent)?;
            let attributes: HashMap<_, _> = abci_event
                .attributes
                .iter()
                .map(|tag| (tag.key.to_string(), tag.value.to_string()))
                .collect();
            Ok(Self {
                event_type,
                attributes,
            })
        }
    }

    impl From<IbcShieldedTransfer> for Memo {
        fn from(shielded: IbcShieldedTransfer) -> Self {
            let bytes =
                shielded.try_to_vec().expect("Encoding shouldn't failed");
            HEXLOWER.encode(&bytes).into()
        }
    }

    impl TryFrom<Memo> for IbcShieldedTransfer {
        type Error = Error;

        fn try_from(memo: Memo) -> Result<Self> {
            let bytes = HEXLOWER
                .decode(memo.as_ref().as_bytes())
                .map_err(Error::DecodingHex)?;
            Self::try_from_slice(&bytes)
                .map_err(Error::DecodingShieldedTransfer)
        }
    }

    /// Get the shielded transfer from the memo
    pub fn get_shielded_transfer(
        event: &IbcEvent,
    ) -> Result<Option<IbcShieldedTransfer>> {
        if event.event_type != "fungible_token_packet" {
            // This event is not for receiving a token
            return Ok(None);
        }
        let is_success =
            event.attributes.get("success") == Some(&"true".to_string());
        let receiver = event.attributes.get("receiver");
        let is_shielded = if let Some(receiver) = receiver {
            PaymentAddress::from_str(&receiver).is_ok()
        } else {
            false
        };
        if !is_success || !is_shielded {
            return Ok(None);
        }

        event
            .attributes
            .get("memo")
            .map(|memo| IbcShieldedTransfer::try_from(Memo::from(memo.clone())))
            .transpose()
    }
}

#[cfg(any(feature = "abciplus", feature = "abcipp"))]
pub use ibc_rs_conversion::*;
