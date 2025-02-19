use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::convert::TryFrom;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;

#[cfg(feature = "ferveo-tpke")]
use ark_ec::AffineCurve;
#[cfg(feature = "ferveo-tpke")]
use ark_ec::PairingEngine;
use borsh::schema::{Declaration, Definition};
use borsh::{BorshDeserialize, BorshSchema, BorshSerialize};
use data_encoding::HEXUPPER;
use masp_primitives::transaction::builder::Builder;
use masp_primitives::transaction::components::sapling::builder::SaplingMetadata;
use masp_primitives::transaction::Transaction;
use masp_primitives::zip32::ExtendedFullViewingKey;
use prost::Message;
use serde::de::Error as SerdeError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::generated::types;
use crate::ledger::gas::{GasMetering, VpGasMeter, VERIFY_TX_SIG_GAS_COST};
use crate::ledger::storage::{KeccakHasher, Sha256Hasher, StorageHasher};
use crate::ledger::testnet_pow;
#[cfg(any(feature = "tendermint", feature = "tendermint-abcipp"))]
use crate::tendermint_proto::abci::ResponseDeliverTx;
use crate::types::account::AccountPublicKeysMap;
use crate::types::address::Address;
use crate::types::chain::ChainId;
use crate::types::keccak::{keccak_hash, KeccakHash};
use crate::types::key::{self, *};
use crate::types::storage::Epoch;
use crate::types::time::DateTimeUtc;
use crate::types::token::MaspDenom;
#[cfg(feature = "ferveo-tpke")]
use crate::types::token::Transfer;
#[cfg(feature = "ferveo-tpke")]
use crate::types::transaction::protocol::ProtocolTx;
#[cfg(feature = "ferveo-tpke")]
use crate::types::transaction::EllipticCurve;
#[cfg(feature = "ferveo-tpke")]
use crate::types::transaction::EncryptionKey;
#[cfg(feature = "ferveo-tpke")]
use crate::types::transaction::WrapperTxErr;
use crate::types::transaction::{
    hash_tx, DecryptedTx, Fee, GasLimit, TxType, WrapperTx,
};

#[derive(Error, Debug)]
pub enum Error {
    #[error("Error decoding a transaction from bytes: {0}")]
    TxDecodingError(prost::DecodeError),
    #[error("Error deserializing transaction field bytes: {0}")]
    TxDeserializingError(std::io::Error),
    #[error("Error deserializing transaction")]
    OfflineTxDeserializationError,
    #[error("Error decoding an DkgGossipMessage from bytes: {0}")]
    DkgDecodingError(prost::DecodeError),
    #[error("Dkg is empty")]
    NoDkgError,
    #[error("Timestamp is empty")]
    NoTimestampError,
    #[error("Timestamp is invalid: {0}")]
    InvalidTimestamp(prost_types::TimestampError),
    #[error("The section signature is invalid: {0}")]
    InvalidSectionSignature(String),
    #[error("Couldn't serialize transaction from JSON at {0}")]
    InvalidJSONDeserialization(String),
    #[error("The wrapper signature is invalid.")]
    InvalidWrapperSignature,
    #[error("Signature verification went out of gas")]
    OutOfGas,
}

pub type Result<T> = std::result::Result<T, Error>;

/// This can be used to sign an arbitrary tx. The signature is produced and
/// verified on the tx data concatenated with the tx code, however the tx code
/// itself is not part of this structure.
///
/// Because the signature is not checked by the ledger, we don't inline it into
/// the `Tx` type directly. Instead, the signature is attached to the `tx.data`,
/// which can then be checked by a validity predicate wasm.
#[derive(Clone, Debug, BorshSerialize, BorshDeserialize, BorshSchema)]
pub struct SignedTxData {
    /// The original tx data bytes, if any
    pub data: Option<Vec<u8>>,
    /// The signature is produced on the tx data concatenated with the tx code
    /// and the timestamp.
    pub sig: common::Signature,
}

/// A serialization method to provide to [`Signed`], such
/// that we may sign serialized data.
///
/// This is a higher level version of [`key::SignableBytes`].
pub trait Signable<T> {
    /// A byte vector containing the serialized data.
    type Output: key::SignableBytes;

    /// The hashing algorithm to use to sign serialized
    /// data with.
    type Hasher: 'static + StorageHasher;

    /// Encodes `data` as a byte vector, with some arbitrary serialization
    /// method.
    ///
    /// The returned output *must* be deterministic based on
    /// `data`, so that two callers signing the same `data` will be
    /// signing the same `Self::Output`.
    fn as_signable(data: &T) -> Self::Output;
}

/// Tag type that indicates we should use [`BorshSerialize`]
/// to sign data in a [`Signed`] wrapper.
#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct SerializeWithBorsh;

/// Tag type that indicates we should use ABI serialization
/// to sign data in a [`Signed`] wrapper.
#[derive(Eq, PartialEq, Clone, Debug, Serialize, Deserialize)]
pub struct SignableEthMessage;

impl<T: BorshSerialize> Signable<T> for SerializeWithBorsh {
    type Hasher = Sha256Hasher;
    type Output = Vec<u8>;

    fn as_signable(data: &T) -> Vec<u8> {
        data.try_to_vec()
            .expect("Encoding data for signing shouldn't fail")
    }
}

impl Signable<KeccakHash> for SignableEthMessage {
    type Hasher = KeccakHasher;
    type Output = KeccakHash;

    fn as_signable(hash: &KeccakHash) -> KeccakHash {
        keccak_hash({
            let mut eth_message = Vec::from("\x19Ethereum Signed Message:\n32");
            eth_message.extend_from_slice(hash.as_ref());
            eth_message
        })
    }
}

/// A generic signed data wrapper for serialize-able types.
///
/// The default serialization method is [`BorshSerialize`].
#[derive(
    Clone, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct Signed<T, S = SerializeWithBorsh> {
    /// Arbitrary data to be signed
    pub data: T,
    /// The signature of the data
    pub sig: common::Signature,
    /// The method to serialize the data with,
    /// before it being signed
    _serialization: PhantomData<S>,
}

impl<S, T: Eq> Eq for Signed<T, S> {}

impl<S, T: PartialEq> PartialEq for Signed<T, S> {
    fn eq(&self, other: &Self) -> bool {
        self.data == other.data && self.sig == other.sig
    }
}

impl<S, T: Hash> Hash for Signed<T, S> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.data.hash(state);
        self.sig.hash(state);
    }
}

impl<S, T: PartialOrd> PartialOrd for Signed<T, S> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.data.partial_cmp(&other.data)
    }
}

impl<S, T: BorshSchema> BorshSchema for Signed<T, S> {
    fn add_definitions_recursively(
        definitions: &mut HashMap<Declaration, Definition>,
    ) {
        let fields = borsh::schema::Fields::NamedFields(borsh::maybestd::vec![
            ("data".to_string(), T::declaration()),
            ("sig".to_string(), <common::Signature>::declaration())
        ]);
        let definition = borsh::schema::Definition::Struct { fields };
        Self::add_definition(Self::declaration(), definition, definitions);
        T::add_definitions_recursively(definitions);
        <common::Signature>::add_definitions_recursively(definitions);
    }

    fn declaration() -> borsh::schema::Declaration {
        format!("Signed<{}>", T::declaration())
    }
}

impl<T, S> Signed<T, S> {
    /// Initialize a new [`Signed`] instance from an existing signature.
    #[inline]
    pub fn new_from(data: T, sig: common::Signature) -> Self {
        Self {
            data,
            sig,
            _serialization: PhantomData,
        }
    }
}

impl<T, S: Signable<T>> Signed<T, S> {
    /// Initialize a new [`Signed`] instance.
    pub fn new(keypair: &common::SecretKey, data: T) -> Self {
        let to_sign = S::as_signable(&data);
        let sig =
            common::SigScheme::sign_with_hasher::<S::Hasher>(keypair, to_sign);
        Self::new_from(data, sig)
    }

    /// Verify that the data has been signed by the secret key
    /// counterpart of the given public key.
    pub fn verify(
        &self,
        pk: &common::PublicKey,
    ) -> std::result::Result<(), VerifySigError> {
        let signed_bytes = S::as_signable(&self.data);
        common::SigScheme::verify_signature_with_hasher::<S::Hasher>(
            pk,
            &signed_bytes,
            &self.sig,
        )
    }
}

/// A section representing transaction data
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub struct Data {
    pub salt: [u8; 8],
    pub data: Vec<u8>,
}

impl Data {
    /// Make a new data section with the given bytes
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            salt: DateTimeUtc::now().0.timestamp_millis().to_le_bytes(),
            data,
        }
    }

    /// Hash this data section
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(
            self.try_to_vec().expect("unable to serialize data section"),
        );
        hasher
    }
}

/// Error representing the case where the supplied code has incorrect hash
pub struct CommitmentError;

/// Represents either some code bytes or their SHA-256 hash
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub enum Commitment {
    /// Result of applying hash function to bytes
    Hash(crate::types::hash::Hash),
    /// Result of applying identity function to bytes
    Id(Vec<u8>),
}

impl Commitment {
    /// Substitute bytes with their SHA-256 hash
    pub fn contract(&mut self) {
        if let Self::Id(code) = self {
            *self = Self::Hash(hash_tx(code));
        }
    }

    /// Substitute a code hash with the supplied bytes if the hashes are
    /// consistent, otherwise return an error
    pub fn expand(
        &mut self,
        code: Vec<u8>,
    ) -> std::result::Result<(), CommitmentError> {
        match self {
            Self::Id(c) if *c == code => Ok(()),
            Self::Hash(hash) if *hash == hash_tx(&code) => {
                *self = Self::Id(code);
                Ok(())
            }
            _ => Err(CommitmentError),
        }
    }

    /// Return the contained hash commitment
    pub fn hash(&self) -> crate::types::hash::Hash {
        match self {
            Self::Id(code) => hash_tx(code),
            Self::Hash(hash) => *hash,
        }
    }

    /// Return the result of applying identity function if there is any
    pub fn id(&self) -> Option<Vec<u8>> {
        if let Self::Id(code) = self {
            Some(code.clone())
        } else {
            None
        }
    }
}

/// A section representing transaction code
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub struct Code {
    /// Additional random data
    pub salt: [u8; 8],
    /// Actual transaction code
    pub code: Commitment,
}

impl Code {
    /// Make a new code section with the given bytes
    pub fn new(code: Vec<u8>) -> Self {
        Self {
            salt: DateTimeUtc::now().0.timestamp_millis().to_le_bytes(),
            code: Commitment::Id(code),
        }
    }

    /// Make a new code section with the given hash
    pub fn from_hash(hash: crate::types::hash::Hash) -> Self {
        Self {
            salt: DateTimeUtc::now().0.timestamp_millis().to_le_bytes(),
            code: Commitment::Hash(hash),
        }
    }

    /// Hash this code section
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(self.salt);
        hasher.update(self.code.hash());
        hasher
    }
}

#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
    Eq,
    PartialEq,
)]
pub struct SignatureIndex {
    pub signature: common::Signature,
    pub index: u8,
}

impl SignatureIndex {
    pub fn from_single_signature(signature: common::Signature) -> Self {
        Self {
            signature,
            index: 0,
        }
    }

    pub fn to_vec(&self) -> Vec<Self> {
        vec![self.clone()]
    }

    pub fn verify(
        &self,
        public_key_index_map: &AccountPublicKeysMap,
        data: &impl SignableBytes,
    ) -> std::result::Result<(), VerifySigError> {
        let public_key =
            public_key_index_map.get_public_key_from_index(self.index);
        if let Some(public_key) = public_key {
            common::SigScheme::verify_signature(
                &public_key,
                data,
                &self.signature,
            )
        } else {
            Err(VerifySigError::MissingData)
        }
    }

    pub fn serialize(&self) -> String {
        let signature_bytes =
            self.try_to_vec().expect("Signature should be serializable");
        HEXUPPER.encode(&signature_bytes)
    }

    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if let Ok(hex) = serde_json::from_slice::<String>(data) {
            match HEXUPPER.decode(hex.as_bytes()) {
                Ok(bytes) => Self::try_from_slice(&bytes)
                    .map_err(Error::TxDeserializingError),
                Err(_) => Err(Error::OfflineTxDeserializationError),
            }
        } else {
            Err(Error::OfflineTxDeserializationError)
        }
    }
}

impl Ord for SignatureIndex {
    fn cmp(&self, other: &Self) -> Ordering {
        self.index.cmp(&other.index)
    }
}

impl PartialOrd for SignatureIndex {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A section representing a multisig over another section
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub struct MultiSignature {
    /// The hash of the section being signed
    pub targets: Vec<crate::types::hash::Hash>,
    /// The signature over the above hash
    pub signatures: BTreeSet<SignatureIndex>,
}

impl MultiSignature {
    /// Sign the given section hash with the given key and return a section
    pub fn new(
        targets: Vec<crate::types::hash::Hash>,
        secret_keys: &[common::SecretKey],
        public_keys_index_map: &AccountPublicKeysMap,
    ) -> Self {
        let target = Self {
            targets: targets.clone(),
            signatures: BTreeSet::new(),
        }
        .get_hash();

        let signatures_public_keys_map =
            secret_keys.iter().map(|secret_key: &common::SecretKey| {
                let signature = common::SigScheme::sign(secret_key, target);
                let public_key = secret_key.ref_to();
                (public_key, signature)
            });

        let signatures = signatures_public_keys_map
            .filter_map(|(public_key, signature)| {
                let public_key_index = public_keys_index_map
                    .get_index_from_public_key(&public_key);
                public_key_index
                    .map(|index| SignatureIndex { signature, index })
            })
            .collect::<BTreeSet<SignatureIndex>>();

        Self {
            targets,
            signatures,
        }
    }

    pub fn total_signatures(&self) -> u8 {
        self.signatures.len() as u8
    }

    /// Hash this signature section
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(
            self.try_to_vec()
                .expect("unable to serialize multisignature section"),
        );
        hasher
    }

    /// Get the hash of this section
    pub fn get_hash(&self) -> crate::types::hash::Hash {
        crate::types::hash::Hash(
            self.hash(&mut Sha256::new()).finalize_reset().into(),
        )
    }

    pub fn get_raw_hash(&self) -> crate::types::hash::Hash {
        Self {
            signatures: BTreeSet::new(),
            ..self.clone()
        }
        .get_hash()
    }
}

/// A section representing the signature over another section
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub struct Signature {
    /// The hash of the section being signed
    targets: Vec<crate::types::hash::Hash>,
    /// The signature over the above hashes
    pub signature: Option<common::Signature>,
}

impl Signature {
    pub fn new(
        targets: Vec<crate::types::hash::Hash>,
        sec_key: &common::SecretKey,
    ) -> Self {
        let mut sec = Self {
            targets,
            signature: None,
        };
        sec.signature = Some(common::SigScheme::sign(sec_key, sec.get_hash()));
        sec
    }

    /// Hash this signature section
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(
            self.try_to_vec()
                .expect("unable to serialize signature section"),
        );
        hasher
    }

    /// Get the hash of this section
    pub fn get_hash(&self) -> crate::types::hash::Hash {
        crate::types::hash::Hash(
            self.hash(&mut Sha256::new()).finalize_reset().into(),
        )
    }

    /// Verify that the signature contained in this section is valid
    pub fn verify_signature(
        &self,
        public_key: &common::PublicKey,
    ) -> std::result::Result<(), VerifySigError> {
        let signature =
            self.signature.as_ref().ok_or(VerifySigError::MissingData)?;
        common::SigScheme::verify_signature(
            public_key,
            &Self {
                signature: None,
                ..self.clone()
            }
            .get_hash(),
            signature,
        )
    }
}

/// Represents a section obtained by encrypting another section
#[derive(Clone, Debug, Serialize, Deserialize)]
#[cfg_attr(feature = "ferveo-tpke", serde(from = "SerializedCiphertext"))]
#[cfg_attr(feature = "ferveo-tpke", serde(into = "SerializedCiphertext"))]
#[cfg_attr(
    not(feature = "ferveo-tpke"),
    derive(BorshSerialize, BorshDeserialize, BorshSchema)
)]
pub struct Ciphertext {
    /// The ciphertext corresponding to the original section serialization
    #[cfg(feature = "ferveo-tpke")]
    pub ciphertext: tpke::Ciphertext<EllipticCurve>,
    /// Ciphertext representation when ferveo not available
    #[cfg(not(feature = "ferveo-tpke"))]
    pub opaque: Vec<u8>,
}

impl Ciphertext {
    /// Make a ciphertext section based on the given sections. Note that this
    /// encryption is not idempotent
    #[cfg(feature = "ferveo-tpke")]
    pub fn new(sections: Vec<Section>, pubkey: &EncryptionKey) -> Self {
        let mut rng = rand::thread_rng();
        let bytes =
            sections.try_to_vec().expect("unable to serialize sections");
        Self {
            ciphertext: tpke::encrypt(&bytes, pubkey.0, &mut rng),
        }
    }

    /// Decrypt this ciphertext back to the original plaintext sections.
    #[cfg(feature = "ferveo-tpke")]
    pub fn decrypt(
        &self,
        privkey: <EllipticCurve as PairingEngine>::G2Affine,
    ) -> std::io::Result<Vec<Section>> {
        let bytes = tpke::decrypt(&self.ciphertext, privkey);
        Vec::<Section>::try_from_slice(&bytes)
    }

    /// Get the hash of this ciphertext section. This operation is done in such
    /// a way it matches the hash of the type pun
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(
            self.try_to_vec().expect("unable to serialize decrypted tx"),
        );
        hasher
    }
}

#[cfg(feature = "ferveo-tpke")]
impl borsh::ser::BorshSerialize for Ciphertext {
    fn serialize<W: std::io::Write>(
        &self,
        writer: &mut W,
    ) -> std::io::Result<()> {
        use ark_serialize::CanonicalSerialize;
        let tpke::Ciphertext {
            nonce,
            ciphertext,
            auth_tag,
        } = &self.ciphertext;
        // Serialize the nonce into bytes
        let mut nonce_buffer = Vec::<u8>::new();
        nonce.serialize(&mut nonce_buffer).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, err)
        })?;
        // serialize the auth_tag to bytes
        let mut tag_buffer = Vec::<u8>::new();
        auth_tag.serialize(&mut tag_buffer).map_err(|err| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, err)
        })?;
        let mut payload = Vec::new();
        // serialize the three byte arrays
        BorshSerialize::serialize(
            &(nonce_buffer, ciphertext, tag_buffer),
            &mut payload,
        )?;
        // now serialize the ciphertext payload with length
        BorshSerialize::serialize(&payload, writer)
    }
}

#[cfg(feature = "ferveo-tpke")]
impl borsh::BorshDeserialize for Ciphertext {
    fn deserialize(buf: &mut &[u8]) -> std::io::Result<Self> {
        type VecTuple = (u32, Vec<u8>, Vec<u8>, Vec<u8>);
        let (_length, nonce, ciphertext, auth_tag): VecTuple =
            BorshDeserialize::deserialize(buf)?;
        Ok(Self {
            ciphertext: tpke::Ciphertext {
                nonce: ark_serialize::CanonicalDeserialize::deserialize(
                    &*nonce,
                )
                .map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
                })?,
                ciphertext,
                auth_tag: ark_serialize::CanonicalDeserialize::deserialize(
                    &*auth_tag,
                )
                .map_err(|err| {
                    std::io::Error::new(std::io::ErrorKind::InvalidData, err)
                })?,
            },
        })
    }
}

#[cfg(feature = "ferveo-tpke")]
impl borsh::BorshSchema for Ciphertext {
    fn add_definitions_recursively(
        definitions: &mut std::collections::HashMap<
            borsh::schema::Declaration,
            borsh::schema::Definition,
        >,
    ) {
        // Encoded as `(Vec<u8>, Vec<u8>, Vec<u8>)`
        let elements = "u8".into();
        let definition = borsh::schema::Definition::Sequence { elements };
        definitions.insert("Vec<u8>".into(), definition);
        let elements =
            vec!["Vec<u8>".into(), "Vec<u8>".into(), "Vec<u8>".into()];
        let definition = borsh::schema::Definition::Tuple { elements };
        definitions.insert(Self::declaration(), definition);
    }

    fn declaration() -> borsh::schema::Declaration {
        "Ciphertext".into()
    }
}

/// A helper struct for serializing EncryptedTx structs
/// as an opaque blob
#[cfg(feature = "ferveo-tpke")]
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
struct SerializedCiphertext {
    payload: Vec<u8>,
}

#[cfg(feature = "ferveo-tpke")]
impl From<Ciphertext> for SerializedCiphertext {
    fn from(tx: Ciphertext) -> Self {
        SerializedCiphertext {
            payload: tx
                .try_to_vec()
                .expect("Unable to serialize encrypted transaction"),
        }
    }
}

#[cfg(feature = "ferveo-tpke")]
impl From<SerializedCiphertext> for Ciphertext {
    fn from(ser: SerializedCiphertext) -> Self {
        BorshDeserialize::deserialize(&mut ser.payload.as_ref())
            .expect("Unable to deserialize encrypted transactions")
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TransactionSerde(Vec<u8>);

impl From<Vec<u8>> for TransactionSerde {
    fn from(tx: Vec<u8>) -> Self {
        Self(tx)
    }
}

impl From<TransactionSerde> for Vec<u8> {
    fn from(tx: TransactionSerde) -> Vec<u8> {
        tx.0
    }
}

fn borsh_serde<T, S>(
    obj: &impl BorshSerialize,
    ser: S,
) -> std::result::Result<S::Ok, S::Error>
where
    S: serde::Serializer,
    T: From<Vec<u8>>,
    T: serde::Serialize,
{
    Into::<T>::into(obj.try_to_vec().unwrap()).serialize(ser)
}

fn serde_borsh<'de, T, S, U>(ser: S) -> std::result::Result<U, S::Error>
where
    S: serde::Deserializer<'de>,
    T: Into<Vec<u8>>,
    T: serde::Deserialize<'de>,
    U: BorshDeserialize,
{
    BorshDeserialize::try_from_slice(&Into::<Vec<u8>>::into(T::deserialize(
        ser,
    )?))
    .map_err(S::Error::custom)
}

/// A structure to facilitate Serde (de)serializations of Builders
#[derive(serde::Serialize, serde::Deserialize)]
struct BuilderSerde(Vec<u8>);

impl From<Vec<u8>> for BuilderSerde {
    fn from(tx: Vec<u8>) -> Self {
        Self(tx)
    }
}

impl From<BuilderSerde> for Vec<u8> {
    fn from(tx: BuilderSerde) -> Vec<u8> {
        tx.0
    }
}

/// A structure to facilitate Serde (de)serializations of SaplingMetadata
#[derive(serde::Serialize, serde::Deserialize)]
pub struct SaplingMetadataSerde(Vec<u8>);

impl From<Vec<u8>> for SaplingMetadataSerde {
    fn from(tx: Vec<u8>) -> Self {
        Self(tx)
    }
}

impl From<SaplingMetadataSerde> for Vec<u8> {
    fn from(tx: SaplingMetadataSerde) -> Vec<u8> {
        tx.0
    }
}

/// A section providing the auxiliary inputs used to construct a MASP
/// transaction
#[derive(
    Clone, Debug, BorshSerialize, BorshDeserialize, Serialize, Deserialize,
)]
pub struct MaspBuilder {
    /// The MASP transaction that this section witnesses
    pub target: crate::types::hash::Hash,
    /// The decoded set of asset types used by the transaction. Useful for
    /// offline wallets trying to display AssetTypes.
    pub asset_types: HashSet<(Address, MaspDenom, Epoch)>,
    /// Track how Info objects map to descriptors and outputs
    #[serde(
        serialize_with = "borsh_serde::<SaplingMetadataSerde, _>",
        deserialize_with = "serde_borsh::<SaplingMetadataSerde, _, _>"
    )]
    pub metadata: SaplingMetadata,
    /// The data that was used to construct the target transaction
    #[serde(
        serialize_with = "borsh_serde::<BuilderSerde, _>",
        deserialize_with = "serde_borsh::<BuilderSerde, _, _>"
    )]
    pub builder: Builder<(), (), ExtendedFullViewingKey, ()>,
}

impl MaspBuilder {
    /// Get the hash of this ciphertext section. This operation is done in such
    /// a way it matches the hash of the type pun
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(
            self.try_to_vec().expect("unable to serialize MASP builder"),
        );
        hasher
    }
}

impl borsh::BorshSchema for MaspBuilder {
    fn add_definitions_recursively(
        _definitions: &mut std::collections::HashMap<
            borsh::schema::Declaration,
            borsh::schema::Definition,
        >,
    ) {
    }

    fn declaration() -> borsh::schema::Declaration {
        "Builder".into()
    }
}

/// A section of a transaction. Carries an independent piece of information
/// necessary for the processing of a transaction.
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub enum Section {
    /// Transaction data that needs to be sent to hardware wallets
    Data(Data),
    /// Transaction data that does not need to be sent to hardware wallets
    ExtraData(Code),
    /// Transaction code. Sending to hardware wallets optional
    Code(Code),
    /// A transaction signature. Often produced by hardware wallets
    SectionSignature(MultiSignature),
    /// A transaction header/protocol signature
    Signature(Signature),
    /// Ciphertext obtained by encrypting arbitrary transaction sections
    Ciphertext(Ciphertext),
    /// Embedded MASP transaction section
    #[serde(
        serialize_with = "borsh_serde::<TransactionSerde, _>",
        deserialize_with = "serde_borsh::<TransactionSerde, _, _>"
    )]
    MaspTx(Transaction),
    /// A section providing the auxiliary inputs used to construct a MASP
    /// transaction. Only send to wallet, never send to protocol.
    MaspBuilder(MaspBuilder),
    /// Wrap a header with a section for the purposes of computing hashes
    Header(Header),
}

impl Section {
    /// Hash this section. Section hashes are useful for signatures and also for
    /// allowing transaction sections to cross reference.
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        // Get the index corresponding to this variant
        let discriminant =
            self.try_to_vec().expect("sections should serialize")[0];
        // Use Borsh's discriminant in the Section's hash
        hasher.update([discriminant]);
        match self {
            Self::Data(data) => data.hash(hasher),
            Self::ExtraData(extra) => extra.hash(hasher),
            Self::Code(code) => code.hash(hasher),
            Self::Signature(signature) => signature.hash(hasher),
            Self::SectionSignature(signatures) => signatures.hash(hasher),
            Self::Ciphertext(ct) => ct.hash(hasher),
            Self::MaspBuilder(mb) => mb.hash(hasher),
            Self::MaspTx(tx) => {
                hasher.update(tx.txid().as_ref());
                hasher
            }
            Self::Header(header) => header.hash(hasher),
        }
    }

    /// Get the hash of this section
    pub fn get_hash(&self) -> crate::types::hash::Hash {
        crate::types::hash::Hash(
            self.hash(&mut Sha256::new()).finalize_reset().into(),
        )
    }

    /// Extract the data from this section if possible
    pub fn data(&self) -> Option<Data> {
        if let Self::Data(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the extra data from this section if possible
    pub fn extra_data_sec(&self) -> Option<Code> {
        if let Self::ExtraData(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the extra data from this section if possible
    pub fn extra_data(&self) -> Option<Vec<u8>> {
        if let Self::ExtraData(data) = self {
            data.code.id()
        } else {
            None
        }
    }

    /// Extract the code from this section is possible
    pub fn code_sec(&self) -> Option<Code> {
        if let Self::Code(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the code from this section is possible
    pub fn code(&self) -> Option<Vec<u8>> {
        if let Self::Code(data) = self {
            data.code.id()
        } else {
            None
        }
    }

    /// Extract the signature from this section if possible
    pub fn signature(&self) -> Option<Signature> {
        if let Self::Signature(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the section signature from this section if possible
    pub fn section_signature(&self) -> Option<MultiSignature> {
        if let Self::SectionSignature(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the ciphertext from this section if possible
    pub fn ciphertext(&self) -> Option<Ciphertext> {
        if let Self::Ciphertext(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the MASP transaction from this section if possible
    pub fn masp_tx(&self) -> Option<Transaction> {
        if let Self::MaspTx(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }

    /// Extract the MASP builder from this section if possible
    pub fn masp_builder(&self) -> Option<MaspBuilder> {
        if let Self::MaspBuilder(data) = self {
            Some(data.clone())
        } else {
            None
        }
    }
}

/// A Namada transaction header indicating where transaction subcomponents can
/// be found
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub struct Header {
    /// The chain which this transaction is being submitted to
    pub chain_id: ChainId,
    /// The time at which this transaction expires
    pub expiration: Option<DateTimeUtc>,
    /// A transaction timestamp
    pub timestamp: DateTimeUtc,
    /// The SHA-256 hash of the transaction's code section
    pub code_hash: crate::types::hash::Hash,
    /// The SHA-256 hash of the transaction's data section
    pub data_hash: crate::types::hash::Hash,
    /// The type of this transaction
    pub tx_type: TxType,
}

impl Header {
    /// Make a new header of the given transaction type
    pub fn new(tx_type: TxType) -> Self {
        Self {
            tx_type,
            chain_id: ChainId::default(),
            expiration: None,
            timestamp: DateTimeUtc::now(),
            code_hash: crate::types::hash::Hash::default(),
            data_hash: crate::types::hash::Hash::default(),
        }
    }

    /// Get the hash of this transaction header.
    pub fn hash<'a>(&self, hasher: &'a mut Sha256) -> &'a mut Sha256 {
        hasher.update(
            self.try_to_vec()
                .expect("unable to serialize transaction header"),
        );
        hasher
    }

    /// Get the wrapper header if it is present
    pub fn wrapper(&self) -> Option<WrapperTx> {
        if let TxType::Wrapper(wrapper) = &self.tx_type {
            Some(*wrapper.clone())
        } else {
            None
        }
    }

    /// Get the decrypted header if it is present
    pub fn decrypted(&self) -> Option<DecryptedTx> {
        if let TxType::Decrypted(decrypted) = &self.tx_type {
            Some(decrypted.clone())
        } else {
            None
        }
    }

    #[cfg(feature = "ferveo-tpke")]
    /// Get the protocol header if it is present
    pub fn protocol(&self) -> Option<ProtocolTx> {
        if let TxType::Protocol(protocol) = &self.tx_type {
            Some(*protocol.clone())
        } else {
            None
        }
    }
}

/// Errors relating to decrypting a wrapper tx and its
/// encrypted payload from a Tx type
#[allow(missing_docs)]
#[derive(thiserror::Error, Debug, PartialEq)]
pub enum TxError {
    #[error("{0}")]
    Unsigned(String),
    #[error("{0}")]
    SigError(String),
    #[error("Failed to deserialize Tx: {0}")]
    Deserialization(String),
}

/// A Namada transaction is represented as a header followed by a series of
/// seections providing additional details.
#[derive(
    Clone,
    Debug,
    BorshSerialize,
    BorshDeserialize,
    BorshSchema,
    Serialize,
    Deserialize,
)]
pub struct Tx {
    /// Type indicating how to process transaction
    pub header: Header,
    /// Additional details necessary to process transaction
    pub sections: Vec<Section>,
}

/// Deserialize Tx from protobufs
impl TryFrom<&[u8]> for Tx {
    type Error = Error;

    fn try_from(tx_bytes: &[u8]) -> Result<Self> {
        let tx = types::Tx::decode(tx_bytes).map_err(Error::TxDecodingError)?;
        BorshDeserialize::try_from_slice(&tx.data)
            .map_err(Error::TxDeserializingError)
    }
}

impl Default for Tx {
    fn default() -> Self {
        Self {
            header: Header::new(TxType::Raw),
            sections: vec![],
        }
    }
}

impl Tx {
    /// Initialize a new transaction builder
    pub fn new(chain_id: ChainId, expiration: Option<DateTimeUtc>) -> Self {
        Tx {
            sections: vec![],
            header: Header {
                chain_id,
                expiration,
                ..Header::new(TxType::Raw)
            },
        }
    }

    /// Create a transaction of the given type
    pub fn from_type(header: TxType) -> Self {
        Tx {
            header: Header::new(header),
            sections: vec![],
        }
    }

    /// Serialize tx to hex string
    pub fn serialize(&self) -> String {
        let tx_bytes = self
            .try_to_vec()
            .expect("Transation should be serializable");
        HEXUPPER.encode(&tx_bytes)
    }

    // Deserialize from hex encoding
    pub fn deserialize(data: &[u8]) -> Result<Self> {
        if let Ok(hex) = serde_json::from_slice::<String>(data) {
            match HEXUPPER.decode(hex.as_bytes()) {
                Ok(bytes) => Tx::try_from_slice(&bytes)
                    .map_err(Error::TxDeserializingError),
                Err(_) => Err(Error::OfflineTxDeserializationError),
            }
        } else {
            Err(Error::OfflineTxDeserializationError)
        }
    }

    /// Get the transaction header
    pub fn header(&self) -> Header {
        self.header.clone()
    }

    /// Get the transaction header hash
    pub fn header_hash(&self) -> crate::types::hash::Hash {
        Section::Header(self.header.clone()).get_hash()
    }

    /// Get hashes of all the sections in this transaction
    pub fn sechashes(&self) -> Vec<crate::types::hash::Hash> {
        let mut hashes = vec![self.header_hash()];
        for sec in &self.sections {
            hashes.push(sec.get_hash());
        }
        hashes
    }

    /// Update the header whilst maintaining existing cross-references
    pub fn update_header(&mut self, tx_type: TxType) -> &mut Self {
        self.header.tx_type = tx_type;
        self
    }

    /// Get the transaction section with the given hash
    pub fn get_section(
        &self,
        hash: &crate::types::hash::Hash,
    ) -> Option<Cow<Section>> {
        if self.header_hash() == *hash {
            return Some(Cow::Owned(Section::Header(self.header.clone())));
        }
        for section in &self.sections {
            if section.get_hash() == *hash {
                return Some(Cow::Borrowed(section));
            }
        }
        None
    }

    /// Add a new section to the transaction
    pub fn add_section(&mut self, section: Section) -> &mut Section {
        self.sections.push(section);
        self.sections.last_mut().unwrap()
    }

    /// Get the hash of this transaction's code from the heeader
    pub fn code_sechash(&self) -> &crate::types::hash::Hash {
        &self.header.code_hash
    }

    /// Set the transaction code hash stored in the header
    pub fn set_code_sechash(&mut self, hash: crate::types::hash::Hash) {
        self.header.code_hash = hash
    }

    /// Get the code designated by the transaction code hash in the header
    pub fn code(&self) -> Option<Vec<u8>> {
        match self
            .get_section(self.code_sechash())
            .as_ref()
            .map(Cow::as_ref)
        {
            Some(Section::Code(section)) => section.code.id(),
            _ => None,
        }
    }

    /// Add the given code to the transaction and set code hash in the header
    pub fn set_code(&mut self, code: Code) -> &mut Section {
        let sec = Section::Code(code);
        self.set_code_sechash(sec.get_hash());
        self.sections.push(sec);
        self.sections.last_mut().unwrap()
    }

    /// Get the transaction data hash stored in the header
    pub fn data_sechash(&self) -> &crate::types::hash::Hash {
        &self.header.data_hash
    }

    /// Set the transaction data hash stored in the header
    pub fn set_data_sechash(&mut self, hash: crate::types::hash::Hash) {
        self.header.data_hash = hash
    }

    /// Add the given code to the transaction and set the hash in the header
    pub fn set_data(&mut self, data: Data) -> &mut Section {
        let sec = Section::Data(data);
        self.set_data_sechash(sec.get_hash());
        self.sections.push(sec);
        self.sections.last_mut().unwrap()
    }

    /// Get the data designated by the transaction data hash in the header
    pub fn data(&self) -> Option<Vec<u8>> {
        match self
            .get_section(self.data_sechash())
            .as_ref()
            .map(Cow::as_ref)
        {
            Some(Section::Data(data)) => Some(data.data.clone()),
            _ => None,
        }
    }

    /// Convert this transaction into protobufs
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![];
        let tx: types::Tx = types::Tx {
            data: self.try_to_vec().expect("encoding a transaction failed"),
        };
        tx.encode(&mut bytes)
            .expect("encoding a transaction failed");
        bytes
    }

    /// Get the inner section hashes
    pub fn inner_section_targets(&self) -> Vec<crate::types::hash::Hash> {
        let mut sections_hashes = self
            .sections
            .iter()
            .filter_map(|section| match section {
                Section::Data(_) | Section::Code(_) => Some(section.get_hash()),
                _ => None,
            })
            .collect::<Vec<crate::types::hash::Hash>>();
        sections_hashes.sort();
        sections_hashes
    }

    /// Verify that the section with the given hash has been signed by the given
    /// public key
    pub fn verify_section_signatures(
        &self,
        hashes: &[crate::types::hash::Hash],
        public_keys_index_map: AccountPublicKeysMap,
        threshold: u8,
        max_signatures: Option<u8>,
        gas_meter: &mut VpGasMeter,
    ) -> std::result::Result<(), Error> {
        let max_signatures = max_signatures.unwrap_or(u8::MAX);
        let mut valid_signatures = 0;

        for section in &self.sections {
            if let Section::SectionSignature(signatures) = section {
                if !hashes.iter().all(|x| {
                    signatures.targets.contains(x) || section.get_hash() == *x
                }) {
                    return Err(Error::InvalidSectionSignature(
                        "missing target hash.".to_string(),
                    ));
                }

                for target in &signatures.targets {
                    if self.get_section(target).is_none() {
                        return Err(Error::InvalidSectionSignature(
                            "Missing target section.".to_string(),
                        ));
                    }
                }

                if signatures.total_signatures() > max_signatures {
                    return Err(Error::InvalidSectionSignature(
                        "too many signatures.".to_string(),
                    ));
                }

                if signatures.total_signatures() < threshold {
                    return Err(Error::InvalidSectionSignature(
                        "too few signatures.".to_string(),
                    ));
                }

                for signature_index in &signatures.signatures {
                    let is_valid_signature = signature_index
                        .verify(
                            &public_keys_index_map,
                            &signatures.get_raw_hash(),
                        )
                        .is_ok();
                    gas_meter
                        .consume(VERIFY_TX_SIG_GAS_COST)
                        .map_err(|_| Error::OutOfGas)?;
                    if is_valid_signature {
                        valid_signatures += 1;
                    }
                    if valid_signatures >= threshold {
                        return Ok(());
                    }
                }
            }
        }
        Err(Error::InvalidSectionSignature(
            "invalid signatures.".to_string(),
        ))
    }

    /// Verify that the sections with the given hashes have been signed together
    /// by the given public key. I.e. this function looks for one signature that
    /// covers over the given slice of hashes.
    pub fn verify_signature(
        &self,
        public_key: &common::PublicKey,
        hashes: &[crate::types::hash::Hash],
    ) -> Result<&Signature> {
        for section in &self.sections {
            if let Section::Signature(signature) = section {
                // Check that the hashes being
                // checked are a subset of those in this section
                if hashes.iter().all(|x| {
                    signature.targets.contains(x) || section.get_hash() == *x
                }) {
                    // Ensure that all the sections the signature signs over are
                    // present
                    for target in &signature.targets {
                        if self.get_section(target).is_none() {
                            return Err(Error::InvalidSectionSignature(
                                "Target section is missing.".to_string(),
                            ));
                        }
                    }
                    // Finally verify that the signature itself is valid
                    return signature
                        .verify_signature(public_key)
                        .map(|_| signature)
                        .map_err(|_| Error::InvalidWrapperSignature);
                }
            }
        }
        Err(Error::InvalidWrapperSignature)
    }

    /// Validate any and all ciphertexts stored in this transaction
    #[cfg(feature = "ferveo-tpke")]
    pub fn validate_ciphertext(&self) -> bool {
        let mut valid = true;
        for section in &self.sections {
            if let Section::Ciphertext(ct) = section {
                valid = valid && ct.ciphertext.check(
                    &<EllipticCurve as PairingEngine>::G1Prepared::from(
                        -<EllipticCurve as PairingEngine>::G1Affine::prime_subgroup_generator(),
                    )
                );
            }
        }
        valid
    }

    pub fn compute_section_signature(
        &self,
        secret_keys: &[common::SecretKey],
        public_keys_index_map: &AccountPublicKeysMap,
    ) -> BTreeSet<SignatureIndex> {
        let targets = self.inner_section_targets();
        MultiSignature::new(targets, secret_keys, public_keys_index_map)
            .signatures
    }

    /// Decrypt any and all ciphertexts stored in this transaction use the
    /// given decryption key
    #[cfg(feature = "ferveo-tpke")]
    pub fn decrypt(
        &mut self,
        privkey: <EllipticCurve as PairingEngine>::G2Affine,
    ) -> std::result::Result<(), WrapperTxErr> {
        // Iterate backwrds to sidestep the effects of deletion on indexing
        for i in (0..self.sections.len()).rev() {
            if let Section::Ciphertext(ct) = &self.sections[i] {
                // Add all the deecrypted sections
                self.sections.extend(
                    ct.decrypt(privkey).map_err(|_| WrapperTxErr::InvalidTx)?,
                );
                // Remove the original ciphertext
                self.sections.remove(i);
            }
        }
        self.data().ok_or(WrapperTxErr::DecryptedHash)?;
        self.get_section(self.code_sechash())
            .ok_or(WrapperTxErr::DecryptedHash)?;
        Ok(())
    }

    /// Encrypt all sections in this transaction other than the header and
    /// signatures over it
    #[cfg(feature = "ferveo-tpke")]
    pub fn encrypt(&mut self, pubkey: &EncryptionKey) -> &mut Self {
        use crate::types::hash::Hash;
        let header_hash = self.header_hash();
        let mut plaintexts = vec![];
        // Iterate backwrds to sidestep the effects of deletion on indexing
        for i in (0..self.sections.len()).rev() {
            match &self.sections[i] {
                Section::Signature(sig)
                    if sig.targets.contains(&header_hash) => {}
                Section::MaspTx(_) => {
                    // Do NOT encrypt the fee unshielding transaction
                    if let Some(unshield_section_hash) = self
                        .header()
                        .wrapper()
                        .expect("Tried to encrypt a non-wrapper tx")
                        .unshield_section_hash
                    {
                        if unshield_section_hash
                            == Hash(
                                self.sections[i]
                                    .hash(&mut Sha256::new())
                                    .finalize_reset()
                                    .into(),
                            )
                        {
                            continue;
                        }
                    }

                    plaintexts.push(self.sections.remove(i))
                }
                // Add eligible section to the list of sections to encrypt
                _ => plaintexts.push(self.sections.remove(i)),
            }
        }
        // Encrypt all eligible sections in one go
        self.sections
            .push(Section::Ciphertext(Ciphertext::new(plaintexts, pubkey)));
        self
    }

    /// Determines the type of the input Tx
    ///
    /// If it is a raw Tx, signed or not, the Tx is
    /// returned unchanged inside an enum variant stating its type.
    ///
    /// If it is a decrypted tx, signing it adds no security so we
    /// extract the signed data without checking the signature (if it
    /// is signed) or return as is. Either way, it is returned in
    /// an enum variant stating its type.
    ///
    /// If it is a WrapperTx, we extract the signed data of
    /// the Tx and verify it is of the appropriate form. This means
    /// 1. The wrapper tx is indeed signed
    /// 2. The signature is valid
    pub fn validate_tx(
        &self,
    ) -> std::result::Result<Option<&Signature>, TxError> {
        match &self.header.tx_type {
            // verify signature and extract signed data
            TxType::Wrapper(wrapper) => self
                .verify_signature(&wrapper.pk, &self.sechashes())
                .map(Option::Some)
                .map_err(|err| {
                    TxError::SigError(format!(
                        "WrapperTx signature verification failed: {}",
                        err
                    ))
                }),
            // verify signature and extract signed data
            #[cfg(feature = "ferveo-tpke")]
            TxType::Protocol(protocol) => self
                .verify_signature(&protocol.pk, &self.sechashes())
                .map(Option::Some)
                .map_err(|err| {
                    TxError::SigError(format!(
                        "ProtocolTx signature verification failed: {}",
                        err
                    ))
                }),
            // we extract the signed data, but don't check the signature
            TxType::Decrypted(_) => Ok(None),
            // return as is
            TxType::Raw => Ok(None),
        }
    }

    /// Filter out all the sections that must not be submitted to the protocol
    /// and return them.
    pub fn protocol_filter(&mut self) -> Vec<Section> {
        let mut filtered = Vec::new();
        for i in (0..self.sections.len()).rev() {
            if let Section::MaspBuilder(_) = self.sections[i] {
                // MASP Builders containin extended full viewing keys amongst
                // other private information and must be removed prior to
                // submission to protocol
                filtered.push(self.sections.remove(i));
            }
        }
        filtered
    }

    /// Filter out all the sections that need not be sent to the hardware wallet
    /// and return them
    pub fn wallet_filter(&mut self) -> Vec<Section> {
        let mut filtered = Vec::new();
        for i in (0..self.sections.len()).rev() {
            match &mut self.sections[i] {
                // This section is known to be large and can be contracted
                Section::Code(section) => {
                    filtered.push(Section::Code(section.clone()));
                    section.code.contract();
                }
                // This section is known to be large and can be contracted
                Section::ExtraData(section) => {
                    filtered.push(Section::ExtraData(section.clone()));
                    section.code.contract();
                }
                // Everything else is fine to add
                _ => {}
            }
        }
        filtered
    }

    /// Add an extra section to the tx builder by hash
    pub fn add_extra_section_from_hash(
        &mut self,
        hash: crate::types::hash::Hash,
    ) -> crate::types::hash::Hash {
        let sechash = self
            .add_section(Section::ExtraData(Code::from_hash(hash)))
            .get_hash();
        sechash
    }

    /// Add an extra section to the tx builder by code
    pub fn add_extra_section(
        &mut self,
        code: Vec<u8>,
    ) -> (&mut Self, crate::types::hash::Hash) {
        let sechash = self
            .add_section(Section::ExtraData(Code::new(code)))
            .get_hash();
        (self, sechash)
    }

    /// Add a masp tx section to the tx builder
    pub fn add_masp_tx_section(
        &mut self,
        tx: Transaction,
    ) -> (&mut Self, crate::types::hash::Hash) {
        let sechash = self.add_section(Section::MaspTx(tx)).get_hash();
        (self, sechash)
    }

    /// Add a masp builder section to the tx builder
    pub fn add_masp_builder(&mut self, builder: MaspBuilder) -> &mut Self {
        let _sec = self.add_section(Section::MaspBuilder(builder));
        self
    }

    /// Add wasm code to the tx builder from hash
    pub fn add_code_from_hash(
        &mut self,
        code_hash: crate::types::hash::Hash,
    ) -> &mut Self {
        self.set_code(Code::from_hash(code_hash));
        self
    }

    /// Add wasm code to the tx builder
    pub fn add_code(&mut self, code: Vec<u8>) -> &mut Self {
        self.set_code(Code::new(code));
        self
    }

    /// Add wasm data to the tx builder
    pub fn add_data(&mut self, data: impl BorshSerialize) -> &mut Self {
        let bytes = data.try_to_vec().expect("Encoding tx data shouldn't fail");
        self.set_data(Data::new(bytes));
        self
    }

    /// Add wasm data already serialized to the tx builder
    pub fn add_serialized_data(&mut self, bytes: Vec<u8>) -> &mut Self {
        self.set_data(Data::new(bytes));
        self
    }

    /// Add wrapper tx to the tx builder
    pub fn add_wrapper(
        &mut self,
        fee: Fee,
        fee_payer: common::PublicKey,
        epoch: Epoch,
        gas_limit: GasLimit,
        #[cfg(not(feature = "mainnet"))] requires_pow: Option<
            testnet_pow::Solution,
        >,
        fee_unshield_hash: Option<crate::types::hash::Hash>,
    ) -> &mut Self {
        self.header.tx_type = TxType::Wrapper(Box::new(WrapperTx::new(
            fee,
            fee_payer,
            epoch,
            gas_limit,
            #[cfg(not(feature = "mainnet"))]
            requires_pow,
            fee_unshield_hash,
        )));
        self
    }

    /// Add fee payer keypair to the tx builder
    pub fn sign_wrapper(&mut self, keypair: common::SecretKey) -> &mut Self {
        self.protocol_filter();
        self.add_section(Section::Signature(Signature::new(
            self.sechashes(),
            &keypair,
        )));
        self
    }

    /// Add signing keys to the tx builder
    pub fn sign_raw(
        &mut self,
        keypairs: Vec<common::SecretKey>,
        account_public_keys_map: AccountPublicKeysMap,
    ) -> &mut Self {
        self.protocol_filter();
        let hashes = self.inner_section_targets();
        self.add_section(Section::SectionSignature(MultiSignature::new(
            hashes,
            &keypairs,
            &account_public_keys_map,
        )));
        self
    }

    /// Add signature
    pub fn add_signatures(
        &mut self,
        signatures: BTreeSet<SignatureIndex>,
    ) -> &mut Self {
        self.protocol_filter();
        self.add_section(Section::SectionSignature(MultiSignature {
            targets: self.inner_section_targets(),
            signatures,
        }));
        self
    }
}

#[cfg(any(feature = "tendermint", feature = "tendermint-abcipp"))]
impl From<Tx> for ResponseDeliverTx {
    #[cfg(not(feature = "ferveo-tpke"))]
    fn from(_tx: Tx) -> ResponseDeliverTx {
        Default::default()
    }

    /// Annotate the Tx with meta-data based on its contents
    #[cfg(feature = "ferveo-tpke")]
    fn from(tx: Tx) -> ResponseDeliverTx {
        use crate::tendermint_proto::abci::{Event, EventAttribute};

        // If data cannot be extracteed, then attach no events
        let tx_data = if let Some(data) = tx.data() {
            data
        } else {
            return Default::default();
        };
        // If the data is not a Transfer, then attach no events
        let transfer = if let Ok(transfer) = Transfer::try_from_slice(&tx_data)
        {
            transfer
        } else {
            return Default::default();
        };
        // Otherwise attach all Transfer events
        let events = vec![Event {
            r#type: "transfer".to_string(),
            attributes: vec![
                EventAttribute {
                    key: "source".to_string(),
                    value: transfer.source.encode(),
                    index: true,
                },
                EventAttribute {
                    key: "target".to_string(),
                    value: transfer.target.encode(),
                    index: true,
                },
                EventAttribute {
                    key: "token".to_string(),
                    value: transfer.token.encode(),
                    index: true,
                },
                EventAttribute {
                    key: "amount".to_string(),
                    value: transfer.amount.to_string(),
                    index: true,
                },
            ],
        }];
        ResponseDeliverTx {
            events,
            info: "Transfer tx".to_string(),
            ..Default::default()
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct DkgGossipMessage {
    pub dkg: Dkg,
}

impl TryFrom<&[u8]> for DkgGossipMessage {
    type Error = Error;

    fn try_from(dkg_bytes: &[u8]) -> Result<Self> {
        let message = types::DkgGossipMessage::decode(dkg_bytes)
            .map_err(Error::DkgDecodingError)?;
        match &message.dkg_message {
            Some(types::dkg_gossip_message::DkgMessage::Dkg(dkg)) => {
                Ok(DkgGossipMessage {
                    dkg: dkg.clone().into(),
                })
            }
            None => Err(Error::NoDkgError),
        }
    }
}

impl From<DkgGossipMessage> for types::DkgGossipMessage {
    fn from(message: DkgGossipMessage) -> Self {
        types::DkgGossipMessage {
            dkg_message: Some(types::dkg_gossip_message::DkgMessage::Dkg(
                message.dkg.into(),
            )),
        }
    }
}

#[allow(dead_code)]
impl DkgGossipMessage {
    pub fn new(dkg: Dkg) -> Self {
        DkgGossipMessage { dkg }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = vec![];
        let message: types::DkgGossipMessage = self.clone().into();
        message
            .encode(&mut bytes)
            .expect("encoding a DKG gossip message failed");
        bytes
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct Dkg {
    pub data: String,
}

impl From<types::Dkg> for Dkg {
    fn from(dkg: types::Dkg) -> Self {
        Dkg { data: dkg.data }
    }
}

impl From<Dkg> for types::Dkg {
    fn from(dkg: Dkg) -> Self {
        types::Dkg { data: dkg.data }
    }
}

#[allow(dead_code)]
impl Dkg {
    pub fn new(data: String) -> Self {
        Dkg { data }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dkg_gossip_message() {
        let data = "arbitrary string".to_owned();
        let dkg = Dkg::new(data);
        let message = DkgGossipMessage::new(dkg);

        let bytes = message.to_bytes();
        let message_from_bytes = DkgGossipMessage::try_from(bytes.as_ref())
            .expect("decoding failed");
        assert_eq!(message_from_bytes, message);
    }

    #[test]
    fn test_dkg() {
        let data = "arbitrary string".to_owned();
        let dkg = Dkg::new(data);

        let types_dkg: types::Dkg = dkg.clone().into();
        let dkg_from_types = Dkg::from(types_dkg);
        assert_eq!(dkg_from_types, dkg);
    }

    /// Test that encryption and decryption are inverses.
    #[cfg(feature = "ferveo-tpke")]
    #[test]
    fn test_encrypt_decrypt() {
        // The trivial public - private keypair
        let pubkey = EncryptionKey(<EllipticCurve as PairingEngine>::G1Affine::prime_subgroup_generator());
        let privkey = <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator();
        // generate encrypted payload
        let plaintext = vec![Section::Data(Data::new(
            "Super secret stuff".as_bytes().to_vec(),
        ))];
        let encrypted = Ciphertext::new(plaintext.clone(), &pubkey);
        // check that encryption doesn't do trivial things
        assert_ne!(
            encrypted.ciphertext.ciphertext,
            plaintext.try_to_vec().expect("Test failed")
        );
        // decrypt the payload and check we got original data back
        let decrypted = encrypted.decrypt(privkey);
        assert_eq!(
            decrypted
                .expect("Test failed")
                .try_to_vec()
                .expect("Test failed"),
            plaintext.try_to_vec().expect("Test failed"),
        );
    }

    /// Test that serializing and deserializing again via Borsh produces
    /// original payload
    #[cfg(feature = "ferveo-tpke")]
    #[test]
    fn test_encrypted_tx_round_trip_borsh() {
        // The trivial public - private keypair
        let pubkey = EncryptionKey(<EllipticCurve as PairingEngine>::G1Affine::prime_subgroup_generator());
        let privkey = <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator();
        // generate encrypted payload
        let plaintext = vec![Section::Data(Data::new(
            "Super secret stuff".as_bytes().to_vec(),
        ))];
        let encrypted = Ciphertext::new(plaintext.clone(), &pubkey);
        // serialize via Borsh
        let borsh = encrypted.try_to_vec().expect("Test failed");
        // deserialize again
        let new_encrypted: Ciphertext =
            BorshDeserialize::deserialize(&mut borsh.as_ref())
                .expect("Test failed");
        // check that decryption works as expected
        let decrypted = new_encrypted.decrypt(privkey);
        assert_eq!(
            decrypted
                .expect("Test failed")
                .try_to_vec()
                .expect("Test failed"),
            plaintext.try_to_vec().expect("Test failed"),
        );
    }

    /// Test that serializing and deserializing again via Serde produces
    /// original payload
    #[cfg(feature = "ferveo-tpke")]
    #[test]
    fn test_encrypted_tx_round_trip_serde() {
        // The trivial public - private keypair
        let pubkey = EncryptionKey(<EllipticCurve as PairingEngine>::G1Affine::prime_subgroup_generator());
        let privkey = <EllipticCurve as PairingEngine>::G2Affine::prime_subgroup_generator();
        // generate encrypted payload
        let plaintext = vec![Section::Data(Data::new(
            "Super secret stuff".as_bytes().to_vec(),
        ))];
        let encrypted = Ciphertext::new(plaintext.clone(), &pubkey);
        // serialize via Serde
        let js = serde_json::to_string(&encrypted).expect("Test failed");
        // deserialize it again
        let new_encrypted: Ciphertext =
            serde_json::from_str(&js).expect("Test failed");
        let decrypted = new_encrypted.decrypt(privkey);
        // check that decryption works as expected
        assert_eq!(
            decrypted
                .expect("Test failed")
                .try_to_vec()
                .expect("Test failed"),
            plaintext.try_to_vec().expect("Test failed"),
        );
    }
}
