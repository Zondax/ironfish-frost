/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use crate::checksum::Checksum;
use crate::checksum::ChecksumError;
use crate::checksum::ChecksumHasher;
use crate::checksum::CHECKSUM_LEN;
use crate::frost::keys::SigningShare;
use crate::frost::round1::NonceCommitment;
use crate::frost::round1::SigningCommitments;
use crate::nonces::deterministic_signing_nonces;
use crate::participant::Identity;
use crate::participant::Secret;
use crate::participant::Signature;
use crate::participant::SignatureError;
use crate::participant::IDENTITY_LEN;
use std::borrow::Borrow;
use std::hash::Hasher;
use std::io;

const NONCE_COMMITMENT_LEN: usize = 32;
pub const AUTHENTICATED_DATA_LEN: usize = IDENTITY_LEN + NONCE_COMMITMENT_LEN * 2 + CHECKSUM_LEN;
pub const SIGNING_COMMITMENT_LEN: usize = AUTHENTICATED_DATA_LEN + Signature::BYTE_SIZE;

#[must_use]
fn input_checksum<I>(transaction_hash: &[u8], signing_participants: &[I]) -> Checksum
where
    I: Borrow<Identity>,
{
    let mut signing_participants = signing_participants
        .iter()
        .map(Borrow::borrow)
        .collect::<Vec<_>>();
    signing_participants.sort_unstable();
    signing_participants.dedup();

    let mut hasher = ChecksumHasher::new();
    hasher.write(transaction_hash);

    for id in signing_participants {
        hasher.write(&id.serialize());
    }

    hasher.finish()
}

#[must_use]
fn authenticated_data(
    identity: &Identity,
    raw_commitments: &SigningCommitments,
    checksum: Checksum,
) -> [u8; AUTHENTICATED_DATA_LEN] {
    let mut data = [0u8; AUTHENTICATED_DATA_LEN];
    let parts = [
        &identity.serialize()[..],
        &raw_commitments.hiding().serialize(),
        &raw_commitments.binding().serialize(),
        &checksum.to_le_bytes(),
    ];
    let mut slice = &mut data[..];
    for part in parts {
        slice[..part.len()].copy_from_slice(part);
        slice = &mut slice[part.len()..];
    }
    debug_assert_eq!(slice.len(), 0);
    data
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SigningCommitment {
    identity: Identity,
    raw_commitments: SigningCommitments,
    /// A checksum of the transaction hash and the signers for a signing operation. Used to quickly
    /// tell if a set of commitments were all generated from the same inputs.
    checksum: Checksum,
    /// Signature that ensures that `hiding`, `binding`, and `checksum` were generated by the owner
    /// of `identity`.
    signature: Signature,
}

impl SigningCommitment {
    fn from_raw_parts(
        identity: Identity,
        raw_commitments: SigningCommitments,
        checksum: Checksum,
        signature: Signature,
    ) -> Result<Self, SignatureError> {
        let signing_commitment = Self {
            identity,
            raw_commitments,
            checksum,
            signature,
        };
        signing_commitment
            .verify_authenticity()
            .map(|_| signing_commitment)
    }

    #[must_use]
    pub fn from_secrets<I>(
        participant_secret: &Secret,
        secret_share: &SigningShare,
        transaction_hash: &[u8],
        signing_participants: &[I],
    ) -> SigningCommitment
    where
        I: Borrow<Identity>,
    {
        let identity = participant_secret.to_identity();
        let nonces =
            deterministic_signing_nonces(secret_share, transaction_hash, signing_participants);
        let raw_commitments = *nonces.commitments();
        let checksum = input_checksum(transaction_hash, signing_participants);
        let authenticated_data = authenticated_data(&identity, &raw_commitments, checksum);
        let signature = participant_secret.sign(&authenticated_data);
        SigningCommitment {
            identity,
            raw_commitments,
            checksum,
            signature,
        }
    }

    pub fn verify_authenticity(&self) -> Result<(), SignatureError> {
        let authenticated_data =
            authenticated_data(&self.identity, &self.raw_commitments, self.checksum);
        self.identity
            .verify_data(&authenticated_data, &self.signature)
    }

    pub fn verify_checksum<I>(
        &self,
        transaction_hash: &[u8],
        signing_participants: &[I],
    ) -> Result<(), ChecksumError>
    where
        I: Borrow<Identity>,
    {
        let computed_checksum = input_checksum(transaction_hash, signing_participants);
        if self.checksum == computed_checksum {
            Ok(())
        } else {
            Err(ChecksumError::SigningCommitmentError)
        }
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn raw_commitments(&self) -> &SigningCommitments {
        &self.raw_commitments
    }

    pub fn hiding(&self) -> &NonceCommitment {
        self.raw_commitments.hiding()
    }

    pub fn binding(&self) -> &NonceCommitment {
        self.raw_commitments.binding()
    }

    pub fn checksum(&self) -> Checksum {
        self.checksum
    }

    pub fn serialize(&self) -> [u8; SIGNING_COMMITMENT_LEN] {
        let mut bytes = [0u8; SIGNING_COMMITMENT_LEN];
        self.serialize_into(&mut bytes)
            .expect("serialization failed");
        bytes
    }

    pub fn serialize_into<W: io::Write>(&self, mut writer: W) -> io::Result<()> {
        writer.write_all(&self.signature.to_bytes())?;
        writer.write_all(&self.identity.serialize())?;
        writer.write_all(&self.hiding().serialize())?;
        writer.write_all(&self.binding().serialize())?;
        writer.write_all(&self.checksum.to_le_bytes())?;
        Ok(())
    }

    pub fn deserialize_from<R: io::Read>(mut reader: R) -> io::Result<Self> {
        let mut signature_bytes = [0u8; Signature::BYTE_SIZE];
        reader.read_exact(&mut signature_bytes)?;
        let signature = Signature::from_bytes(&signature_bytes);

        let identity = Identity::deserialize_from(&mut reader)?;

        let mut hiding = [0u8; 32];
        reader.read_exact(&mut hiding)?;
        let hiding = NonceCommitment::deserialize(hiding).map_err(io::Error::other)?;

        let mut binding = [0u8; 32];
        reader.read_exact(&mut binding)?;
        let binding = NonceCommitment::deserialize(binding).map_err(io::Error::other)?;

        let raw_commitments = SigningCommitments::new(hiding, binding);

        let mut checksum = [0u8; 8];
        reader.read_exact(&mut checksum)?;
        let checksum = Checksum::from_le_bytes(checksum);

        Self::from_raw_parts(identity, raw_commitments, checksum, signature)
            .map_err(io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::authenticated_data;
    use super::SigningCommitment;
    use crate::frost::keys::SigningShare;
    use crate::participant::Secret;
    use hex_literal::hex;
    use rand::thread_rng;

    #[test]
    fn serialization_round_trip() {
        let mut rng = thread_rng();

        let secret = Secret::random(&mut rng);
        let signing_share = SigningShare::default();
        let signing_participants = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            b"transaction hash",
            &signing_participants,
        );

        let serialized = commitment.serialize();

        let deserialized =
            SigningCommitment::deserialize_from(&serialized[..]).expect("deserialization failed");

        assert_eq!(deserialized, commitment);
    }

    #[test]
    fn deserialization_regression() {
        let serialization = hex!(
            "
            307be5a2c20495d05966fc12b2cee3ea4d44cb3623f92b0f6a391c626fa7708e835
            26e886448d5ef376c5d09675aed3e711cd3e0df9f6c607604e6a7371a210e725c3a
            20a22aebc59d856bfbaa48fde8f8ea6fe48ddd978555932c283e760397f78b4b468
            2f9b70f8baad6d7752f5e25bcbc6b3453d16d92589da722ad13a7390d0057c6aae8
            363a50e835b89b44bccdd5889ef5a362fa89d841c96e65b34dbe3adf8f71faa041f
            394ef6b127c4b6b1e43714f32c450e8d3d089b376915acd6500639cad9b202c479e
            4216e2d4d16cad09b634e01270f4a52707d924fd9834e6206f48f04388ae90bcd63
            f901369c6034760245574a2d3068f52b617d33ca1a417ea391d3785b542f5
        "
        );
        let deserialized = SigningCommitment::deserialize_from(&serialization[..])
            .expect("deserialization failed");
        assert_eq!(serialization, deserialized.serialize());
    }

    #[test]
    fn test_invalid_deserialization() {
        let mut rng = thread_rng();

        let secret = Secret::random(&mut rng);
        let signing_share = SigningShare::default();
        let signing_participants = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            b"transaction hash",
            &signing_participants,
        );

        let serialized = commitment.serialize();

        for index in 0..serialized.len() {
            let mut invalid_serialization = serialized;
            invalid_serialization[index] ^= 0xff;
            assert!(SigningCommitment::deserialize_from(&invalid_serialization[..]).is_err());
        }
    }

    #[test]
    fn test_valid_signature() {
        let mut rng = thread_rng();

        let secret = Secret::random(&mut rng);
        let signing_share = SigningShare::default();
        let signing_participants = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            b"transaction hash",
            &signing_participants,
        );

        assert!(commitment.verify_authenticity().is_ok());
    }

    #[test]
    fn test_invalid_signature() {
        let mut rng = thread_rng();

        let secret = Secret::random(&mut rng);
        let signing_share = SigningShare::default();
        let signing_participants = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            b"transaction hash",
            &signing_participants,
        );

        let unrelated_secret = Secret::random(&mut rng);
        let invalid_signature = unrelated_secret.sign(&authenticated_data(
            commitment.identity(),
            commitment.raw_commitments(),
            commitment.checksum(),
        ));

        let invalid_commitment = SigningCommitment {
            identity: commitment.identity().clone(),
            raw_commitments: *commitment.raw_commitments(),
            checksum: commitment.checksum(),
            signature: invalid_signature,
        };

        assert!(invalid_commitment.verify_authenticity().is_err());
    }

    #[test]
    fn test_checksum_stability() {
        let mut rng = thread_rng();

        let secret1 = Secret::random(&mut rng);
        let secret2 = Secret::random(&mut rng);
        let signing_share1 = SigningShare::default();
        let signing_share2 = SigningShare::default();
        let transaction_hash = b"something";
        let signing_participants = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment1 = SigningCommitment::from_secrets(
            &secret1,
            &signing_share1,
            transaction_hash,
            &signing_participants,
        );

        let commitment2 = SigningCommitment::from_secrets(
            &secret2,
            &signing_share2,
            transaction_hash,
            &signing_participants,
        );

        assert_ne!(commitment1, commitment2);
        assert_eq!(commitment1.checksum(), commitment2.checksum());
    }

    #[test]
    fn test_checksum_variation_with_transaction_hash() {
        let mut rng = thread_rng();

        let secret = Secret::random(&mut rng);
        let signing_share = SigningShare::default();
        let signing_participants = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment1 = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            b"something",
            &signing_participants,
        );

        let commitment2 = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            b"something else",
            &signing_participants,
        );

        assert_ne!(commitment1.checksum(), commitment2.checksum());
    }

    #[test]
    fn test_checksum_variation_with_signers_list() {
        let mut rng = thread_rng();

        let secret = Secret::random(&mut rng);
        let signing_share = SigningShare::default();
        let transaction_hash = b"something";
        let signing_participants1 = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];
        let signing_participants2 = [
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
            Secret::random(&mut rng).to_identity(),
        ];

        let commitment1 = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            transaction_hash,
            &signing_participants1,
        );

        let commitment2 = SigningCommitment::from_secrets(
            &secret,
            &signing_share,
            transaction_hash,
            &signing_participants2,
        );

        assert_ne!(commitment1.checksum(), commitment2.checksum());
    }
}
