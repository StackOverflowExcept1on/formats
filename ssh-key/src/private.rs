//! SSH private key support.
//!
//! Support for decoding SSH private keys from the OpenSSH file format:
//!
//! <https://github.com/openssh/openssh-portable/blob/master/PROTOCOL.key>

#[cfg(feature = "alloc")]
mod dsa;
#[cfg(feature = "ecdsa")]
mod ecdsa;
mod ed25519;
#[cfg(feature = "alloc")]
mod rsa;

#[cfg(feature = "ecdsa")]
pub use self::ecdsa::{EcdsaKeypair, EcdsaPrivateKey};
pub use self::ed25519::{Ed25519Keypair, Ed25519PrivateKey};
#[cfg(feature = "alloc")]
pub use self::{
    dsa::{DsaKeypair, DsaPrivateKey},
    rsa::RsaKeypair,
};

use crate::{
    decoder::{Decode, Decoder},
    encoder::{Encode, Encoder},
    public, Algorithm, Cipher, Error, Kdf, PublicKey, Result,
};
use core::str;
use pem_rfc7468::{self as pem, LineEnding, PemLabel};

#[cfg(feature = "alloc")]
use {
    crate::encoder::encoded_len,
    alloc::{string::String, vec::Vec},
    zeroize::Zeroizing,
};

#[cfg(feature = "fingerprint")]
use crate::{Fingerprint, HashAlg};

#[cfg(feature = "encryption")]
use rand_core::{CryptoRng, RngCore};

#[cfg(feature = "std")]
use std::{fs, io::Write, path::Path};

#[cfg(all(unix, feature = "std"))]
use std::os::unix::fs::OpenOptionsExt;

#[cfg(feature = "subtle")]
use subtle::{Choice, ConstantTimeEq};

/// Block size to use for unencrypted keys.
const DEFAULT_BLOCK_SIZE: usize = 8;

/// Maximum supported block size.
///
/// This is the block size used by e.g. AES.
const MAX_BLOCK_SIZE: usize = 16;

/// Padding bytes to use.
const PADDING_BYTES: [u8; MAX_BLOCK_SIZE - 1] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Line width used by the PEM encoding of OpenSSH private keys.
const PEM_LINE_WIDTH: usize = 70;

/// Unix file permissions for SSH private keys.
#[cfg(all(unix, feature = "std"))]
const UNIX_FILE_PERMISSIONS: u32 = 0o600;

/// SSH private key.
#[derive(Clone, Debug)]
pub struct PrivateKey {
    /// Cipher algorithm (a.k.a. `ciphername`).
    cipher: Option<Cipher>,

    /// KDF options.
    kdf: Kdf,

    /// Public key.
    public_key: PublicKey,

    /// Key data.
    key_data: KeypairData,
}

impl PrivateKey {
    /// Magic string used to identify keys in this format.
    const AUTH_MAGIC: &'static [u8] = b"openssh-key-v1\0";

    /// Create a new unencrypted private key with the given keypair data and comment.
    ///
    /// On `no_std` platforms, use `PrivateKey::from(key_data)` instead.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn new(key_data: KeypairData, comment: impl Into<String>) -> Result<Self> {
        if key_data.is_encrypted() {
            return Err(Error::Encrypted);
        }

        let mut private_key = Self::try_from(key_data)?;
        private_key.public_key.comment = comment.into();
        Ok(private_key)
    }

    /// Parse an OpenSSH-formatted PEM private key.
    ///
    /// OpenSSH-formatted private keys begin with the following:
    ///
    /// ```text
    /// -----BEGIN OPENSSH PRIVATE KEY-----
    /// ```
    pub fn from_openssh(input: impl AsRef<[u8]>) -> Result<Self> {
        let mut pem_decoder = pem::Decoder::new_wrapped(input.as_ref(), PEM_LINE_WIDTH)?;
        Self::validate_pem_label(pem_decoder.type_label())?;

        let mut auth_magic = [0u8; Self::AUTH_MAGIC.len()];
        pem_decoder.decode(&mut auth_magic)?;

        if auth_magic != Self::AUTH_MAGIC {
            return Err(Error::FormatEncoding);
        }

        let cipher = Option::<Cipher>::decode(&mut pem_decoder)?;
        let kdf = Kdf::decode(&mut pem_decoder)?;
        let nkeys = pem_decoder.decode_usize()?;

        // TODO(tarcieri): support more than one key?
        if nkeys != 1 {
            return Err(Error::Length);
        }

        #[cfg_attr(not(feature = "alloc"), allow(unused_mut))]
        let mut public_key = PublicKey::from(
            pem_decoder.decode_length_prefixed(|decoder, _len| public::KeyData::decode(decoder))?,
        );

        // Handle encrypted private key
        #[cfg(not(feature = "alloc"))]
        if cipher.is_some() {
            return Err(Error::Encrypted);
        }
        #[cfg(feature = "alloc")]
        if cipher.is_some() {
            let key_data = KeypairData::Encrypted(pem_decoder.decode_byte_vec()?);

            if !pem_decoder.is_finished() {
                return Err(Error::Length);
            }

            return Ok(Self {
                cipher,
                kdf,
                public_key,
                key_data,
            });
        }

        let key_data = pem_decoder.decode_length_prefixed(|decoder, _len| {
            KeypairData::decode_with_comment(decoder, &mut public_key, DEFAULT_BLOCK_SIZE)
        })?;

        Ok(Self {
            cipher,
            kdf,
            public_key,
            key_data,
        })
    }

    /// Encode OpenSSH-formatted (PEM) private key.
    pub fn encode_openssh<'o>(
        &self,
        line_ending: LineEnding,
        out: &'o mut [u8],
    ) -> Result<&'o str> {
        let mut pem_encoder =
            pem::Encoder::new_wrapped(Self::TYPE_LABEL, PEM_LINE_WIDTH, line_ending, out)?;

        pem_encoder.encode(Self::AUTH_MAGIC)?;
        self.cipher.encode(&mut pem_encoder)?;
        self.kdf.encode(&mut pem_encoder)?;

        // TODO(tarcieri): support for encoding more than one private key
        let nkeys = 1;
        pem_encoder.encode_usize(nkeys)?;

        // Encode public key
        pem_encoder.encode_length_prefixed(self.public_key.key_data())?;

        // Encode private key
        pem_encoder.encode_usize(self.private_key_len(DEFAULT_BLOCK_SIZE)?)?;

        if self.is_encrypted() {
            self.key_data.encode(&mut pem_encoder)?;
        } else {
            self.key_data.encode_with_comment(
                &mut pem_encoder,
                self.comment(),
                DEFAULT_BLOCK_SIZE,
            )?;
        }

        let encoded_len = pem_encoder.finish()?;
        Ok(str::from_utf8(&out[..encoded_len])?)
    }

    /// Encode an OpenSSH-formatted PEM private key, allocating a
    /// self-zeroizing [`String`] for the result.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn to_openssh(&self, line_ending: LineEnding) -> Result<Zeroizing<String>> {
        let encoded_len = self.openssh_encoded_len(line_ending)?;
        let mut buf = vec![0u8; encoded_len];
        let actual_len = self.encode_openssh(line_ending, &mut buf)?.len();
        buf.truncate(actual_len);
        Ok(Zeroizing::new(String::from_utf8(buf)?))
    }

    /// Read private key from an OpenSSH-formatted PEM file.
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    pub fn read_openssh_file(path: &Path) -> Result<Self> {
        // TODO(tarcieri): verify file permissions match `UNIX_FILE_PERMISSIONS`
        let pem = Zeroizing::new(fs::read_to_string(path)?);
        Self::from_openssh(&*pem)
    }

    /// Write private key as an OpenSSH-formatted PEM file.
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    pub fn write_openssh_file(&self, path: &Path, line_ending: LineEnding) -> Result<()> {
        let pem = self.to_openssh(line_ending)?;

        #[cfg(not(unix))]
        fs::write(path, pem.as_bytes())?;
        #[cfg(unix)]
        fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(UNIX_FILE_PERMISSIONS)
            .open(path)
            .and_then(|mut file| file.write_all(pem.as_bytes()))?;

        Ok(())
    }

    /// Attempt to decrypt an encrypted private key using the provided
    /// password to derive an encryption key.
    ///
    /// Returns [`Error::Decrypted`] if the private key is already decrypted.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub fn decrypt(&self, password: impl AsRef<[u8]>) -> Result<Self> {
        let cipher = self.cipher.ok_or(Error::Decrypted)?;
        let (key_bytes, iv_bytes) = self.kdf.derive_key_and_iv(cipher, password)?;

        let mut buffer =
            Zeroizing::new(self.key_data.encrypted().ok_or(Error::Decrypted)?.to_vec());

        cipher.decrypt(&key_bytes, &iv_bytes, buffer.as_mut_slice())?;

        let mut public_key = self.public_key.clone();

        let key_data = KeypairData::decode_with_comment(
            &mut buffer.as_slice(),
            &mut public_key,
            cipher.block_size(),
        )?;

        Ok(Self {
            cipher: None,
            kdf: Kdf::None,
            public_key,
            key_data,
        })
    }

    /// Attempt to encrypt an unencrypted private key using the provided
    /// password to derive an encryption key.
    ///
    /// Uses the following algorithms:
    /// - Cipher: [`Cipher::Aes256Ctr`]
    /// - KDF: [`Kdf::Bcrypt`] (i.e. `bcrypt-pbkdf`)
    ///
    /// Returns [`Error::Encrypted`] if the private key is already encrypted.
    #[cfg(feature = "encryption")]
    #[cfg_attr(docsrs, doc(cfg(feature = "encryption")))]
    pub fn encrypt(
        &self,
        rng: impl CryptoRng + RngCore,
        password: impl AsRef<[u8]>,
    ) -> Result<Self> {
        if self.is_encrypted() {
            return Err(Error::Encrypted);
        }

        let cipher = Cipher::default();
        let kdf = Kdf::new(Default::default(), rng)?;
        let (key_bytes, iv_bytes) = kdf.derive_key_and_iv(cipher, password)?;
        let mut buffer = Vec::with_capacity(self.private_key_len(cipher.block_size())?);

        // Encode and encrypt private key
        self.key_data
            .encode_with_comment(&mut buffer, self.comment(), cipher.block_size())?;
        cipher.encrypt(&key_bytes, &iv_bytes, buffer.as_mut_slice())?;

        Ok(Self {
            cipher: Some(cipher),
            kdf,
            public_key: self.public_key.key_data.clone().into(),
            key_data: KeypairData::Encrypted(buffer),
        })
    }

    /// Get the digital signature [`Algorithm`] used by this key.
    pub fn algorithm(&self) -> Algorithm {
        self.public_key.algorithm()
    }

    /// Comment on the key (e.g. email address).
    pub fn comment(&self) -> &str {
        self.public_key.comment()
    }

    /// Cipher algorithm (a.k.a. `ciphername`).
    pub fn cipher(&self) -> Option<Cipher> {
        self.cipher
    }

    /// Compute key fingerprint.
    ///
    /// Use [`Default::default()`] to use the default hash function (SHA-256).
    #[cfg(feature = "fingerprint")]
    #[cfg_attr(docsrs, doc(cfg(feature = "fingerprint")))]
    pub fn fingerprint(&self, hash_alg: HashAlg) -> Fingerprint {
        self.public_key.fingerprint(hash_alg)
    }

    /// Is this key encrypted?
    pub fn is_encrypted(&self) -> bool {
        let ret = self.key_data.is_encrypted();
        debug_assert_eq!(ret, self.cipher.is_some());
        ret
    }

    /// Key Derivation Function (KDF) used to encrypt this key.
    ///
    /// Returns [`Kdf::None`] if this key is not encrypted.
    pub fn kdf(&self) -> &Kdf {
        &self.kdf
    }

    /// Keypair data.
    pub fn key_data(&self) -> &KeypairData {
        &self.key_data
    }

    /// Get the [`PublicKey`] which corresponds to this private key.
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// Estimated length of a PEM-encoded key in OpenSSH format.
    ///
    /// May be slightly longer than the actual result.
    #[cfg(feature = "alloc")]
    fn openssh_encoded_len(&self, line_ending: LineEnding) -> Result<usize> {
        let private_key_len = self.private_key_len(DEFAULT_BLOCK_SIZE)?;

        // TODO(tarcieri): checked arithmetic
        let bytes_len = Self::AUTH_MAGIC.len()
            + self.cipher.encoded_len()?
            + self.kdf.encoded_len()?
            + 4 // number of keys (encoded as uint32)
            + 4 + self.public_key.key_data().encoded_len()?
            + 4 + private_key_len;

        let mut base64_len = encoded_len(bytes_len);

        // Add the length of the line endings which will be inserted when
        // encoded Base64 is linewrapped
        base64_len += (base64_len.saturating_sub(1) / PEM_LINE_WIDTH) * line_ending.len();

        Ok(pem::encapsulated_len(
            Self::TYPE_LABEL,
            line_ending,
            base64_len,
        ))
    }

    /// Get the length of the private key data in bytes (including padding).
    fn private_key_len(&self, block_size: usize) -> Result<usize> {
        if self.is_encrypted() {
            self.key_data().encoded_len()
        } else {
            let len = self.key_data().encoded_len_with_comment(self.comment())?;
            Ok(len + padding_len(len, block_size))
        }
    }
}

impl TryFrom<KeypairData> for PrivateKey {
    type Error = Error;

    fn try_from(key_data: KeypairData) -> Result<PrivateKey> {
        let public_key = public::KeyData::try_from(&key_data)?;

        Ok(Self {
            cipher: None,
            kdf: Kdf::None,
            public_key: public_key.into(),
            key_data,
        })
    }
}

impl From<PrivateKey> for PublicKey {
    fn from(private_key: PrivateKey) -> PublicKey {
        private_key.public_key
    }
}

impl From<&PrivateKey> for PublicKey {
    fn from(private_key: &PrivateKey) -> PublicKey {
        private_key.public_key.clone()
    }
}

impl PemLabel for PrivateKey {
    const TYPE_LABEL: &'static str = "OPENSSH PRIVATE KEY";
}

impl str::FromStr for PrivateKey {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::from_openssh(s)
    }
}

#[cfg(feature = "subtle")]
#[cfg_attr(docsrs, doc(cfg(feature = "subtle")))]
impl ConstantTimeEq for PrivateKey {
    fn ct_eq(&self, other: &Self) -> Choice {
        // Constant-time with respect to private key data
        self.key_data.ct_eq(&other.key_data)
            & Choice::from(
                (self.cipher == other.cipher
                    && self.kdf == other.kdf
                    && self.public_key == other.public_key) as u8,
            )
    }
}

#[cfg(feature = "subtle")]
#[cfg_attr(docsrs, doc(cfg(feature = "subtle")))]
impl PartialEq for PrivateKey {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other).into()
    }
}

#[cfg(feature = "subtle")]
#[cfg_attr(docsrs, doc(cfg(feature = "subtle")))]
impl Eq for PrivateKey {}

/// Private key data.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum KeypairData {
    /// Digital Signature Algorithm (DSA) keypair.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    Dsa(DsaKeypair),

    /// ECDSA keypair.
    #[cfg(feature = "ecdsa")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ecdsa")))]
    Ecdsa(EcdsaKeypair),

    /// Ed25519 keypair.
    Ed25519(Ed25519Keypair),

    /// Encrypted private key (ciphertext).
    #[cfg(feature = "alloc")]
    Encrypted(Vec<u8>),

    /// RSA keypair.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    Rsa(RsaKeypair),
}

impl KeypairData {
    /// Get the [`Algorithm`] for this private key.
    pub fn algorithm(&self) -> Result<Algorithm> {
        Ok(match self {
            #[cfg(feature = "alloc")]
            Self::Dsa(_) => Algorithm::Dsa,
            #[cfg(feature = "ecdsa")]
            Self::Ecdsa(key) => key.algorithm(),
            Self::Ed25519(_) => Algorithm::Ed25519,
            #[cfg(feature = "alloc")]
            Self::Encrypted(_) => return Err(Error::Encrypted),
            #[cfg(feature = "alloc")]
            Self::Rsa(_) => Algorithm::Rsa,
        })
    }

    /// Get DSA keypair if this key is the correct type.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn dsa(&self) -> Option<&DsaKeypair> {
        match self {
            Self::Dsa(key) => Some(key),
            _ => None,
        }
    }

    /// Get ECDSA private key if this key is the correct type.
    #[cfg(feature = "ecdsa")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ecdsa")))]
    pub fn ecdsa(&self) -> Option<&EcdsaKeypair> {
        match self {
            Self::Ecdsa(keypair) => Some(keypair),
            _ => None,
        }
    }

    /// Get Ed25519 private key if this key is the correct type.
    pub fn ed25519(&self) -> Option<&Ed25519Keypair> {
        match self {
            Self::Ed25519(key) => Some(key),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Get the encrypted ciphertext if this key is encrypted.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn encrypted(&self) -> Option<&[u8]> {
        match self {
            Self::Encrypted(ciphertext) => Some(ciphertext),
            _ => None,
        }
    }

    /// Get RSA keypair if this key is the correct type.
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn rsa(&self) -> Option<&RsaKeypair> {
        match self {
            Self::Rsa(key) => Some(key),
            _ => None,
        }
    }

    /// Is this key a DSA key?
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn is_dsa(&self) -> bool {
        matches!(self, Self::Dsa(_))
    }

    /// Is this key an ECDSA key?
    #[cfg(feature = "ecdsa")]
    #[cfg_attr(docsrs, doc(cfg(feature = "ecdsa")))]
    pub fn is_ecdsa(&self) -> bool {
        matches!(self, Self::Ecdsa(_))
    }

    /// Is this key an Ed25519 key?
    pub fn is_ed25519(&self) -> bool {
        matches!(self, Self::Ed25519(_))
    }

    /// Is this key encrypted?
    #[cfg(not(feature = "alloc"))]
    pub fn is_encrypted(&self) -> bool {
        false
    }

    /// Is this key encrypted?
    #[cfg(feature = "alloc")]
    pub fn is_encrypted(&self) -> bool {
        matches!(self, Self::Encrypted(_))
    }

    /// Is this key an RSA key?
    #[cfg(feature = "alloc")]
    #[cfg_attr(docsrs, doc(cfg(feature = "alloc")))]
    pub fn is_rsa(&self) -> bool {
        matches!(self, Self::Rsa(_))
    }

    /// Decode [`KeypairData`] along with its associated comment, storing
    /// the comment in the provided public key.
    ///
    /// This method also checks padding for validity and ensures that the
    /// decoded private key matches the provided public key.
    ///
    /// For private key format specification, see OpenSSH PROTOCOL.key § 3
    fn decode_with_comment(
        decoder: &mut impl Decoder,
        public_key: &mut PublicKey,
        block_size: usize,
    ) -> Result<Self> {
        debug_assert!(block_size <= MAX_BLOCK_SIZE);

        // Ensure input data is padding-aligned
        if decoder.remaining_len() % block_size != 0 {
            return Err(Error::Length);
        }

        let key_data = KeypairData::decode(decoder)?;

        // Ensure public key matches private key
        if public_key.key_data() != &public::KeyData::try_from(&key_data)? {
            return Err(Error::PublicKey);
        }

        public_key.decode_comment(decoder)?;

        let padding_len = decoder.remaining_len();

        if padding_len >= block_size {
            return Err(Error::Length);
        }

        if padding_len != 0 {
            let mut padding = [0u8; MAX_BLOCK_SIZE];
            decoder.decode_raw(&mut padding[..padding_len])?;

            if PADDING_BYTES[..padding_len] != padding[..padding_len] {
                return Err(Error::FormatEncoding);
            }
        }

        if !decoder.is_finished() {
            return Err(Error::Length);
        }

        Ok(key_data)
    }

    /// Encode [`KeypairData`] along with its associated comment and padding.
    fn encode_with_comment(
        &self,
        encoder: &mut impl Encoder,
        comment: &str,
        block_size: usize,
    ) -> Result<()> {
        if self.is_encrypted() {
            // Can't encode an encrypted key with a comment
            return Err(Error::Encrypted);
        }

        let private_key_len = self.encoded_len_with_comment(comment)?;
        let padding_len = padding_len(private_key_len, block_size);

        self.encode(encoder)?;
        encoder.encode_str(comment)?;
        encoder.encode_raw(&PADDING_BYTES[..padding_len])?;
        Ok(())
    }

    /// Get the length of this private key when encoded with the given comment.
    ///
    /// This length is sans padding.
    fn encoded_len_with_comment(&self, comment: &str) -> Result<usize> {
        // TODO(tarcieri): checked arithmetic
        Ok(self.encoded_len()?
            + 4 // comment length prefix
            + comment.len())
    }
}

impl Decode for KeypairData {
    fn decode(decoder: &mut impl Decoder) -> Result<Self> {
        let checkint1 = decoder.decode_u32()?;
        let checkint2 = decoder.decode_u32()?;

        if checkint1 != checkint2 {
            return Err(Error::Crypto);
        }

        match Algorithm::decode(decoder)? {
            #[cfg(feature = "alloc")]
            Algorithm::Dsa => DsaKeypair::decode(decoder).map(Self::Dsa),
            #[cfg(feature = "ecdsa")]
            Algorithm::Ecdsa(curve) => match EcdsaKeypair::decode(decoder)? {
                keypair if keypair.curve() == curve => Ok(Self::Ecdsa(keypair)),
                _ => Err(Error::Algorithm),
            },
            Algorithm::Ed25519 => Ed25519Keypair::decode(decoder).map(Self::Ed25519),
            #[cfg(feature = "alloc")]
            Algorithm::Rsa => RsaKeypair::decode(decoder).map(Self::Rsa),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Algorithm),
        }
    }
}

impl Encode for KeypairData {
    fn encoded_len(&self) -> Result<usize> {
        let header_len = if self.is_encrypted() {
            0
        } else {
            let checkint_len = 8; // 2 x 32-bit checkints
            checkint_len + self.algorithm()?.encoded_len()?
        };

        let key_len = match self {
            #[cfg(feature = "alloc")]
            Self::Dsa(key) => key.encoded_len()?,
            #[cfg(feature = "ecdsa")]
            Self::Ecdsa(key) => key.encoded_len()?,
            Self::Ed25519(key) => key.encoded_len()?,
            #[cfg(feature = "alloc")]
            Self::Encrypted(ciphertext) => ciphertext.len(),
            #[cfg(feature = "alloc")]
            Self::Rsa(key) => key.encoded_len()?,
        };

        Ok(header_len + key_len)
    }

    fn encode(&self, encoder: &mut impl Encoder) -> Result<()> {
        if !self.is_encrypted() {
            // Compute checkint (uses deterministic method)
            let checkint = public::KeyData::try_from(self)?.checkint();
            encoder.encode_u32(checkint)?;
            encoder.encode_u32(checkint)?;

            self.algorithm()?.encode(encoder)?;
        }

        match self {
            #[cfg(feature = "alloc")]
            Self::Dsa(key) => key.encode(encoder),
            #[cfg(feature = "ecdsa")]
            Self::Ecdsa(key) => key.encode(encoder),
            Self::Ed25519(key) => key.encode(encoder),
            #[cfg(feature = "alloc")]
            Self::Encrypted(ciphertext) => encoder.encode_raw(ciphertext),
            #[cfg(feature = "alloc")]
            Self::Rsa(key) => key.encode(encoder),
        }
    }
}

impl TryFrom<&KeypairData> for public::KeyData {
    type Error = Error;

    fn try_from(keypair_data: &KeypairData) -> Result<public::KeyData> {
        Ok(match keypair_data {
            #[cfg(feature = "alloc")]
            KeypairData::Dsa(dsa) => public::KeyData::Dsa(dsa.into()),
            #[cfg(feature = "ecdsa")]
            KeypairData::Ecdsa(ecdsa) => public::KeyData::Ecdsa(ecdsa.into()),
            KeypairData::Ed25519(ed25519) => public::KeyData::Ed25519(ed25519.into()),
            #[cfg(feature = "alloc")]
            KeypairData::Encrypted(_) => return Err(Error::Encrypted),
            #[cfg(feature = "alloc")]
            KeypairData::Rsa(rsa) => public::KeyData::Rsa(rsa.into()),
        })
    }
}

#[cfg(feature = "subtle")]
#[cfg_attr(docsrs, doc(cfg(feature = "subtle")))]
impl ConstantTimeEq for KeypairData {
    fn ct_eq(&self, other: &Self) -> Choice {
        // Note: constant-time with respect to key *data* comparisons, not algorithms
        match (self, other) {
            #[cfg(feature = "alloc")]
            (Self::Dsa(a), Self::Dsa(b)) => a.ct_eq(b),
            #[cfg(feature = "ecdsa")]
            (Self::Ecdsa(a), Self::Ecdsa(b)) => a.ct_eq(b),
            (Self::Ed25519(a), Self::Ed25519(b)) => a.ct_eq(b),
            #[cfg(feature = "alloc")]
            (Self::Encrypted(a), Self::Encrypted(b)) => a.ct_eq(b),
            #[cfg(feature = "alloc")]
            (Self::Rsa(a), Self::Rsa(b)) => a.ct_eq(b),
            _ => Choice::from(0),
        }
    }
}

#[cfg(feature = "subtle")]
#[cfg_attr(docsrs, doc(cfg(feature = "subtle")))]
impl PartialEq for KeypairData {
    fn eq(&self, other: &Self) -> bool {
        self.ct_eq(other).into()
    }
}

#[cfg(feature = "subtle")]
#[cfg_attr(docsrs, doc(cfg(feature = "subtle")))]
impl Eq for KeypairData {}

/// Compute padding length for the given input length and block size.
fn padding_len(input_size: usize, block_size: usize) -> usize {
    let input_rem = input_size % block_size;

    let padding_len = if input_rem == 0 {
        0
    } else {
        block_size - input_rem
    };

    debug_assert!(
        padding_len < MAX_BLOCK_SIZE,
        "padding too long: {}",
        padding_len
    );

    padding_len
}
