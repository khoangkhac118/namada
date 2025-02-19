#![allow(missing_docs)]

pub mod generated;
mod types;

pub use types::{
    Code, Commitment, Data, Dkg, Error, Header, MaspBuilder, MultiSignature,
    Section, Signable, SignableEthMessage, Signature, SignatureIndex, Signed,
    Tx, TxError,
};

#[cfg(test)]
mod tests {
    use data_encoding::HEXLOWER;
    use generated::types::Tx;
    use prost::Message;

    use super::*;

    #[test]
    fn encoding_round_trip() {
        let tx = Tx {
            data: "arbitrary data".as_bytes().to_owned(),
        };
        let mut tx_bytes = vec![];
        tx.encode(&mut tx_bytes).unwrap();
        let tx_hex = HEXLOWER.encode(&tx_bytes);
        let tx_from_hex = HEXLOWER.decode(tx_hex.as_ref()).unwrap();
        let tx_from_bytes = Tx::decode(&tx_from_hex[..]).unwrap();
        assert_eq!(tx, tx_from_bytes);
    }
}
