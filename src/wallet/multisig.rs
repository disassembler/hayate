// CIP-1854 multi-signature HD wallet support
// https://cips.cardano.org/cip/CIP-1854

use ed25519_bip32::{XPrv, DerivationScheme};
use pallas_addresses::{Address, ShelleyAddress, ShelleyPaymentPart, ShelleyDelegationPart};
use pallas_codec::minicbor;
use pallas_crypto::hash::{Hash, Hasher};
use pallas_crypto::key::ed25519::PublicKey;
use pallas_traverse::ComputeHash;
use thiserror::Error;

use crate::wallet::derivation::Network;

#[derive(Error, Debug)]
pub enum MultisigError {
    #[error("Invalid vkey '{input}': {reason}")]
    InvalidVkey { input: String, reason: String },

    #[error("Invalid threshold: M={m} must be >= 1 and <= N={n}")]
    InvalidThreshold { m: usize, n: usize },

    #[error("CBOR encode error: {0}")]
    CborEncode(String),

    #[error("CBOR decode error: {0}")]
    CborDecode(String),

    #[error("Address error: {0}")]
    AddressError(String),
}

pub type MultisigResult<T> = Result<T, MultisigError>;

/// Derive a CIP-1854 payment key.
/// Path: m/1854'/1815'/account_index'/0/key_index
pub fn derive_multisig_payment_key(root: &XPrv, account_index: u32, key_index: u32) -> XPrv {
    let purpose = root.derive(DerivationScheme::V2, 0x80000000 | 1854);
    let coin_type = purpose.derive(DerivationScheme::V2, 0x80000000 | 1815);
    let account = coin_type.derive(DerivationScheme::V2, 0x80000000 | account_index);
    let role = account.derive(DerivationScheme::V2, 0); // role 0 = payment
    role.derive(DerivationScheme::V2, key_index)
}

/// Parse an external verification key from hex (32 or 64 bytes) or bech32
/// (addr_shared_xvk = 64 bytes extended, addr_shared_vk = 32 bytes).
/// Always returns the 32-byte raw Ed25519 public key.
pub fn parse_vkey(input: &str) -> MultisigResult<[u8; 32]> {
    let bytes = if input.starts_with("addr_shared_xvk") || input.starts_with("addr_shared_vk") {
        use bech32::primitives::decode::UncheckedHrpstring;

        let unchecked = UncheckedHrpstring::new(input).map_err(|e| MultisigError::InvalidVkey {
            input: input.to_string(),
            reason: format!("invalid bech32: {}", e),
        })?;
        let checked = unchecked
            .validate_and_remove_checksum::<bech32::Bech32>()
            .map_err(|e| MultisigError::InvalidVkey {
                input: input.to_string(),
                reason: format!("invalid bech32 checksum: {}", e),
            })?;

        checked.byte_iter().collect::<Vec<u8>>()
    } else {
        hex::decode(input).map_err(|e| MultisigError::InvalidVkey {
            input: input.to_string(),
            reason: format!("invalid hex: {}", e),
        })?
    };

    match bytes.len() {
        32 => Ok(bytes.try_into().unwrap()),
        64 => Ok(bytes[..32].try_into().unwrap()),
        n => Err(MultisigError::InvalidVkey {
            input: input.to_string(),
            reason: format!("expected 32 or 64 bytes, got {}", n),
        }),
    }
}

/// Extract the 28-byte payment key hash from a bech32 Shelley payment address.
/// Works for enterprise (addr1/addr_test1) and base addresses; fails on script addresses.
pub fn payment_keyhash_from_address(addr_bech32: &str) -> MultisigResult<[u8; 28]> {
    let addr = Address::from_bech32(addr_bech32).map_err(|e| MultisigError::AddressError(e.to_string()))?;
    match addr {
        Address::Shelley(shelley) => match shelley.payment() {
            ShelleyPaymentPart::Key(hash) => {
                let bytes: &[u8] = hash.as_ref();
                bytes.try_into().map_err(|_| MultisigError::AddressError("payment key hash is not 28 bytes".into()))
            }
            ShelleyPaymentPart::Script(_) => Err(MultisigError::AddressError(
                "address has a script payment credential, not a key".into(),
            )),
        },
        _ => Err(MultisigError::AddressError(
            "only Shelley addresses are supported".into(),
        )),
    }
}

/// Compute BLAKE2b-224 key hash of a 32-byte Ed25519 public key (28 bytes).
pub fn vkey_hash(pubkey_32: &[u8; 32]) -> [u8; 28] {
    let pubkey = PublicKey::from(*pubkey_32);
    let hash = pubkey.compute_hash();
    let bytes: &[u8] = hash.as_ref();
    bytes.try_into().expect("BLAKE2b-224 is always 28 bytes")
}

/// Encode an M-of-N native script to CBOR.
///
/// Cardano native script grammar:
///   ScriptNOfK = [3, n_required: uint, [ScriptPubkey, ...]]
///   ScriptPubkey = [0, key_hash: bytes(28)]
pub fn encode_native_script_n_of_k(
    threshold: u32,
    key_hashes: &[[u8; 28]],
) -> MultisigResult<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = minicbor::Encoder::new(&mut buf);

    enc.array(3)
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    enc.u8(3) // ScriptNOfK tag
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    enc.u32(threshold)
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    enc.array(key_hashes.len() as u64)
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;

    for hash in key_hashes {
        enc.array(2)
            .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.u8(0) // ScriptPubkey tag
            .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(hash.as_ref())
            .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    }

    Ok(buf)
}

/// Compute native script hash: BLAKE2b-224 with tag byte 0 prepended.
pub fn native_script_hash(script_cbor: &[u8]) -> [u8; 28] {
    let hash: Hash<28> = Hasher::<224>::hash_tagged(script_cbor, 0);
    let bytes: &[u8] = hash.as_ref();
    bytes.try_into().expect("BLAKE2b-224 is always 28 bytes")
}

/// Build a multisig enterprise address from a native script hash.
pub fn multisig_enterprise_address(
    script_hash_28: &[u8; 28],
    network: Network,
) -> MultisigResult<String> {
    let hash = Hash::<28>::from(*script_hash_28);
    let addr = ShelleyAddress::new(
        network.to_pallas(),
        ShelleyPaymentPart::Script(hash),
        ShelleyDelegationPart::Null,
    );
    Address::Shelley(addr)
        .to_bech32()
        .map_err(|e| MultisigError::AddressError(e.to_string()))
}

/// Build a single-key enterprise address from a 28-byte key hash.
/// This is the address to use for portal signer registration — the portal
/// extracts the key hash from this address to link it to the signing key.
pub fn key_enterprise_address(key_hash_28: &[u8; 28], network: Network) -> MultisigResult<String> {
    let hash = Hash::<28>::from(*key_hash_28);
    let addr = ShelleyAddress::new(
        network.to_pallas(),
        ShelleyPaymentPart::Key(hash),
        ShelleyDelegationPart::Null,
    );
    Address::Shelley(addr)
        .to_bech32()
        .map_err(|e| MultisigError::AddressError(e.to_string()))
}

/// Full pipeline: combine local key with external keys, build M-of-N script, return address.
///
/// Returns `(bech32_address, script_cbor, all_key_hashes)`.
/// Key order: local key first, then external keys in supplied order.
pub fn create_multisig_address(
    local_signing_key: &XPrv,
    external_pubkeys: &[[u8; 32]],
    threshold: u32,
    network: Network,
) -> MultisigResult<(String, Vec<u8>, Vec<[u8; 28]>)> {
    let n = 1 + external_pubkeys.len();
    let m = threshold as usize;

    if m == 0 || m > n {
        return Err(MultisigError::InvalidThreshold { m, n });
    }

    let local_pubkey: [u8; 32] = local_signing_key.public().public_key();
    let mut all_hashes: Vec<[u8; 28]> = vec![vkey_hash(&local_pubkey)];
    for ext in external_pubkeys {
        all_hashes.push(vkey_hash(ext));
    }

    let script_cbor = encode_native_script_n_of_k(threshold, &all_hashes)?;
    let hash_28 = native_script_hash(&script_cbor);
    let address = multisig_enterprise_address(&hash_28, network)?;

    Ok((address, script_cbor, all_hashes))
}

/// Extract the tx body bytes from a Conway era tx envelope CBOR.
///
/// If the CBOR is a full tx array `[tx_body, witness_set, bool, aux_data]`,
/// returns the raw bytes of the tx_body map element.
/// If it's already a tx body map, returns it unchanged.
pub fn extract_tx_body(cbor: &[u8]) -> MultisigResult<Vec<u8>> {
    let first = cbor
        .first()
        .ok_or_else(|| MultisigError::CborDecode("empty CBOR".into()))?;

    // 0x84 = definite array(4) — full Conway tx
    if *first == 0x84 {
        let mut d = minicbor::Decoder::new(cbor);
        d.array()
            .map_err(|e| MultisigError::CborDecode(e.to_string()))?;
        let body_start = d.position();
        d.skip()
            .map_err(|e| MultisigError::CborDecode(e.to_string()))?;
        let body_end = d.position();
        Ok(cbor[body_start..body_end].to_vec())
    } else {
        Ok(cbor.to_vec())
    }
}

/// Produce a CIP-8 DataSignature (COSE_Sign1 + COSE_Key) for portal authentication.
///
/// `payload_hex` is the hex-encoded nonce from GET /api/v1/getNonce.
/// `address_bytes` is the raw binary form of the signer's Shelley address.
/// Returns `(signature_cbor_hex, key_cbor_hex)` for POST /api/v1/authSigner.
pub fn sign_cip8(
    payload_hex: &str,
    signing_key: &XPrv,
    address_bytes: &[u8],
) -> MultisigResult<(String, String)> {
    let payload_bytes = hex::decode(payload_hex).map_err(|e| MultisigError::CborEncode(
        format!("invalid nonce hex: {}", e)
    ))?;

    // Build protected header: {1: -8, "address": address_bytes}
    let mut protected = Vec::new();
    {
        let mut enc = minicbor::Encoder::new(&mut protected);
        enc.map(2).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.i32(1).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.i32(-8).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.str("address").map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(address_bytes).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    }

    // Build Sig_structure: ["Signature1", bstr(protected), b"", payload]
    let mut sig_struct = Vec::new();
    {
        let mut enc = minicbor::Encoder::new(&mut sig_struct);
        enc.array(4).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.str("Signature1").map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(&protected).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(b"").map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(&payload_bytes).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    }

    // Sign the Sig_structure with the Ed25519 key
    let sig: ed25519_bip32::Signature<Vec<u8>> = signing_key.sign(&sig_struct);
    let sig_bytes: &[u8] = sig.as_ref();
    let vkey_bytes: [u8; 32] = signing_key.public().public_key();

    // Build COSE_Sign1 (no tag): [bstr(protected), {}, payload, signature]
    let mut cose_sign1 = Vec::new();
    {
        let mut enc = minicbor::Encoder::new(&mut cose_sign1);
        enc.array(4).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(&protected).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.map(0).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(&payload_bytes).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(sig_bytes).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    }

    // Build COSE_Key: {1:1, 3:-8, -1:6, -2:vkey}
    let mut cose_key = Vec::new();
    {
        let mut enc = minicbor::Encoder::new(&mut cose_key);
        enc.map(4).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.i32(1).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.i32(1).map_err(|e| MultisigError::CborEncode(e.to_string()))?;   // kty: OKP
        enc.i32(3).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.i32(-8).map_err(|e| MultisigError::CborEncode(e.to_string()))?;  // alg: EdDSA
        enc.i32(-1).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.i32(6).map_err(|e| MultisigError::CborEncode(e.to_string()))?;   // crv: Ed25519
        enc.i32(-2).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(&vkey_bytes).map_err(|e| MultisigError::CborEncode(e.to_string()))?;  // x: pubkey
    }

    Ok((hex::encode(cose_sign1), hex::encode(cose_key)))
}

/// Embed a single vkey witness into an existing Conway tx CBOR.
///
/// Takes the built (unsigned or partially-signed) tx, adds `[vkey_32, sig_64]`
/// to the vkeywitnesses (key 0) in the witness set, and returns the new tx CBOR.
pub fn embed_vkey_witness(tx_cbor: &[u8], vkey_32: &[u8], sig_64: &[u8]) -> MultisigResult<Vec<u8>> {
    use pallas_codec::minicbor::{Decoder, Encoder};

    let mut dec = Decoder::new(tx_cbor);
    dec.array().map_err(|e| MultisigError::CborDecode(e.to_string()))?;

    let body_start = dec.position();
    dec.skip().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
    let body_bytes = &tx_cbor[body_start..dec.position()];

    let witness_start = dec.position();
    dec.skip().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
    let witness_bytes = &tx_cbor[witness_start..dec.position()];

    let valid = dec.bool().map_err(|e| MultisigError::CborDecode(e.to_string()))?;

    let aux_start = dec.position();
    if aux_start < tx_cbor.len() {
        dec.skip().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
    }
    let aux_bytes = &tx_cbor[aux_start..dec.position()];

    // Parse existing witness set fields
    let mut wdec = Decoder::new(witness_bytes);
    let map_len = wdec.map().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
    let mut fields: std::collections::BTreeMap<u64, Vec<u8>> = std::collections::BTreeMap::new();
    for _ in 0..map_len.unwrap_or(0) {
        let key = wdec.u64().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
        let vs = wdec.position();
        wdec.skip().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
        fields.insert(key, witness_bytes[vs..wdec.position()].to_vec());
    }

    // Build new witness set: key 0 = vkeywitnesses (new or appended), rest unchanged
    let has_vkeys = fields.contains_key(&0);
    let total_fields = if has_vkeys { fields.len() } else { fields.len() + 1 };

    let mut new_witness = Vec::new();
    {
        let mut enc = Encoder::new(&mut new_witness);
        enc.map(total_fields as u64).map_err(|e| MultisigError::CborEncode(e.to_string()))?;

        // Key 0: vkeywitnesses
        enc.u64(0).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        if let Some(existing) = fields.get(&0) {
            let mut vdec = Decoder::new(existing);
            let arr_len = vdec.array().map_err(|e| MultisigError::CborDecode(e.to_string()))?.unwrap_or(0);
            enc.array(arr_len + 1).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
            for _ in 0..arr_len {
                let es = vdec.position();
                vdec.skip().map_err(|e| MultisigError::CborDecode(e.to_string()))?;
                std::io::Write::write_all(enc.writer_mut(), &existing[es..vdec.position()])
                    .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
            }
        } else {
            enc.array(1).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        }
        enc.array(2).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(vkey_32).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bytes(sig_64).map_err(|e| MultisigError::CborEncode(e.to_string()))?;

        // Remaining keys (e.g. native scripts at key 1)
        for (k, v) in &fields {
            if *k == 0 { continue; }
            enc.u64(*k).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
            std::io::Write::write_all(enc.writer_mut(), v)
                .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        }
    }

    // Reassemble [body, witness_set, valid, aux]
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.array(4).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        std::io::Write::write_all(enc.writer_mut(), body_bytes)
            .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        std::io::Write::write_all(enc.writer_mut(), &new_witness)
            .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        enc.bool(valid).map_err(|e| MultisigError::CborEncode(e.to_string()))?;
        std::io::Write::write_all(enc.writer_mut(), aux_bytes)
            .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    }

    Ok(buf)
}

/// Sign a tx body and return (vkey_32_bytes, sig_64_bytes).
/// The vkey is the raw 32-byte Ed25519 public key (not extended).
pub fn sign_multisig_tx(tx_body_cbor: &[u8], signing_key: &XPrv) -> (Vec<u8>, Vec<u8>) {
    let mut hasher = Hasher::<256>::new();
    hasher.input(tx_body_cbor);
    let tx_hash = hasher.finalize();

    let sig: ed25519_bip32::Signature<Vec<u8>> = signing_key.sign(tx_hash.as_ref());
    let vkey_32: [u8; 32] = signing_key.public().public_key();

    (vkey_32.to_vec(), sig.as_ref().to_vec())
}

/// Encode a single key witness as CBOR for use in a cardano-cli TxWitness envelope.
///
/// cardano-cli expects the cborHex of a TxWitness file to be a bare key witness:
///   `[vkey_32_bytes, sig_64_bytes]`  (array(2), NOT a witness-set map)
pub fn encode_key_witness(vkey_32: &[u8], sig_64: &[u8]) -> MultisigResult<Vec<u8>> {
    let mut buf = Vec::new();
    let mut enc = minicbor::Encoder::new(&mut buf);

    enc.array(2)
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    enc.bytes(vkey_32)
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;
    enc.bytes(sig_64)
        .map_err(|e| MultisigError::CborEncode(e.to_string()))?;

    Ok(buf)
}
