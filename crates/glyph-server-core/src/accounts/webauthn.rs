//! The part of WebAuthn a relying party needs: check `clientDataJSON`, parse
//! `authenticatorData` and the attestation object (CBOR), and verify ES256 /
//! RS256 assertion signatures. Attestation statements are not verified: we ask
//! for `attestation: "none"` and only trust the key the authenticator created.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sha2::{Digest, Sha256};

pub const ES256: i64 = -7;
pub const RS256: i64 = -257;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaError {
    Malformed(&'static str),
    WrongType,
    WrongChallenge,
    WrongOrigin,
    WrongRp,
    NoUserPresence,
    NoUserVerification,
    UnsupportedKey,
    BadSignature,
    Replayed,
}

impl std::fmt::Display for WaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WaError::Malformed(what) => write!(f, "malformed {what}"),
            WaError::WrongType => f.write_str("wrong ceremony type"),
            WaError::WrongChallenge => f.write_str("challenge mismatch"),
            WaError::WrongOrigin => f.write_str("origin not allowed"),
            WaError::WrongRp => f.write_str("relying party mismatch"),
            WaError::NoUserPresence => f.write_str("user presence missing"),
            WaError::NoUserVerification => f.write_str("user verification missing"),
            WaError::UnsupportedKey => f.write_str("unsupported key type (use ES256 or RS256)"),
            WaError::BadSignature => f.write_str("signature invalid"),
            WaError::Replayed => f.write_str("signature counter went backwards"),
        }
    }
}

pub fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// base64url with or without padding.
pub fn unb64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

// ------------------------------------------------------------------ CBOR

#[derive(Debug, Clone, PartialEq)]
pub enum Cbor {
    Int(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Cbor>),
    Map(Vec<(Cbor, Cbor)>),
    Bool(bool),
    Null,
    Float,
}

impl Cbor {
    fn get_int(&self, k: i128) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m.iter().find(|(key, _)| *key == Cbor::Int(k)).map(|(_, v)| v),
            _ => None,
        }
    }
    fn get_text(&self, k: &str) -> Option<&Cbor> {
        match self {
            Cbor::Map(m) => m.iter().find(|(key, _)| matches!(key, Cbor::Text(t) if t == k)).map(|(_, v)| v),
            _ => None,
        }
    }
    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Cbor::Bytes(b) => Some(b),
            _ => None,
        }
    }
    fn int(&self) -> Option<i128> {
        match self {
            Cbor::Int(i) => Some(*i),
            _ => None,
        }
    }
}

/// Parse one definite-length CBOR item; returns it and the bytes consumed.
pub fn cbor(buf: &[u8]) -> Result<(Cbor, usize), WaError> {
    parse(buf, 0)
}

fn parse(buf: &[u8], depth: u8) -> Result<(Cbor, usize), WaError> {
    const BAD: WaError = WaError::Malformed("CBOR");
    if depth > 16 {
        return Err(BAD);
    }
    let first = *buf.first().ok_or(BAD)?;
    let (major, info) = (first >> 5, first & 0x1f);
    let (arg, mut pos) = match info {
        0..=23 => (info as u64, 1),
        24..=27 => {
            let n = 1usize << (info - 24);
            let raw = buf.get(1..1 + n).ok_or(BAD)?;
            (raw.iter().fold(0u64, |a, b| (a << 8) | *b as u64), 1 + n)
        }
        _ => return Err(BAD), // indefinite lengths are not used by WebAuthn
    };
    let len = |a: u64| -> Result<usize, WaError> { usize::try_from(a).ok().filter(|&l| l <= buf.len()).ok_or(BAD) };
    let v = match major {
        0 => Cbor::Int(arg as i128),
        1 => Cbor::Int(-1 - arg as i128),
        2 | 3 => {
            let l = len(arg)?;
            let raw = buf.get(pos..pos + l).ok_or(BAD)?;
            pos += l;
            if major == 2 {
                Cbor::Bytes(raw.to_vec())
            } else {
                Cbor::Text(String::from_utf8(raw.to_vec()).map_err(|_| BAD)?)
            }
        }
        4 => {
            let mut v = Vec::new();
            for _ in 0..len(arg)? {
                let (item, used) = parse(buf.get(pos..).ok_or(BAD)?, depth + 1)?;
                pos += used;
                v.push(item);
            }
            Cbor::Array(v)
        }
        5 => {
            let mut v = Vec::new();
            for _ in 0..len(arg)? {
                let (k, used) = parse(buf.get(pos..).ok_or(BAD)?, depth + 1)?;
                pos += used;
                let (val, used) = parse(buf.get(pos..).ok_or(BAD)?, depth + 1)?;
                pos += used;
                v.push((k, val));
            }
            Cbor::Map(v)
        }
        6 => {
            let (inner, used) = parse(buf.get(pos..).ok_or(BAD)?, depth + 1)?;
            pos += used;
            inner
        }
        _ => match info {
            20 => Cbor::Bool(false),
            21 => Cbor::Bool(true),
            22 | 23 => Cbor::Null,
            25..=27 => Cbor::Float,
            _ => return Err(BAD),
        },
    };
    Ok((v, pos))
}

// ------------------------------------------------------------------ ceremonies

pub struct Expect<'a> {
    pub rp_id: &'a str,
    pub origins: &'a [String],
    /// base64url challenge we issued.
    pub challenge: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewCredential {
    pub cred_id: String,
    pub alg: i64,
    /// base64url COSE key.
    pub public_key: String,
    pub sign_count: u32,
}

#[derive(serde::Deserialize)]
struct ClientData {
    #[serde(rename = "type")]
    kind: String,
    challenge: String,
    origin: String,
    #[serde(default, rename = "crossOrigin")]
    cross_origin: bool,
}

fn check_client_data(e: &Expect, raw: &[u8], kind: &str) -> Result<(), WaError> {
    let cd: ClientData = serde_json::from_slice(raw).map_err(|_| WaError::Malformed("clientDataJSON"))?;
    if cd.kind != kind {
        return Err(WaError::WrongType);
    }
    if unb64(&cd.challenge).is_none() || cd.challenge.trim_end_matches('=') != e.challenge {
        return Err(WaError::WrongChallenge);
    }
    if cd.cross_origin || !e.origins.iter().any(|o| o == &cd.origin) {
        return Err(WaError::WrongOrigin);
    }
    Ok(())
}

struct AuthData<'a> {
    flags: u8,
    sign_count: u32,
    rest: &'a [u8],
}

fn auth_data<'a>(e: &Expect, raw: &'a [u8]) -> Result<AuthData<'a>, WaError> {
    if raw.len() < 37 {
        return Err(WaError::Malformed("authenticatorData"));
    }
    if raw[..32] != Sha256::digest(e.rp_id.as_bytes())[..] {
        return Err(WaError::WrongRp);
    }
    let flags = raw[32];
    if flags & 0x01 == 0 {
        return Err(WaError::NoUserPresence);
    }
    if flags & 0x04 == 0 {
        return Err(WaError::NoUserVerification);
    }
    let sign_count = u32::from_be_bytes([raw[33], raw[34], raw[35], raw[36]]);
    Ok(AuthData { flags, sign_count, rest: &raw[37..] })
}

/// `navigator.credentials.create()` result.
pub fn verify_registration(e: &Expect, client_data_json: &[u8], attestation_object: &[u8]) -> Result<NewCredential, WaError> {
    check_client_data(e, client_data_json, "webauthn.create")?;
    let (att, _) = cbor(attestation_object)?;
    let raw = att.get_text("authData").and_then(Cbor::bytes).ok_or(WaError::Malformed("attestationObject"))?;
    let ad = auth_data(e, raw)?;
    if ad.flags & 0x40 == 0 || ad.rest.len() < 18 {
        return Err(WaError::Malformed("attested credential data"));
    }
    let id_len = u16::from_be_bytes([ad.rest[16], ad.rest[17]]) as usize;
    let cred_id = ad.rest.get(18..18 + id_len).ok_or(WaError::Malformed("credential id"))?;
    let key_raw = &ad.rest[18 + id_len..];
    let (key, used) = cbor(key_raw)?;
    let alg = key.get_int(3).and_then(Cbor::int).and_then(|a| i64::try_from(a).ok()).ok_or(WaError::UnsupportedKey)?;
    check_key(&key, alg)?;
    Ok(NewCredential { cred_id: b64(cred_id), alg, public_key: b64(&key_raw[..used]), sign_count: ad.sign_count })
}

fn check_key(key: &Cbor, alg: i64) -> Result<(), WaError> {
    match (key.get_int(1).and_then(Cbor::int), alg) {
        (Some(2), ES256) => {
            es256_key(key)?;
        }
        (Some(3), RS256) => {
            rs256_key(key)?;
        }
        _ => return Err(WaError::UnsupportedKey),
    }
    Ok(())
}

fn es256_key(key: &Cbor) -> Result<p256::ecdsa::VerifyingKey, WaError> {
    let crv = key.get_int(-1).and_then(Cbor::int);
    let x = key.get_int(-2).and_then(Cbor::bytes).filter(|b| b.len() == 32);
    let y = key.get_int(-3).and_then(Cbor::bytes).filter(|b| b.len() == 32);
    let (Some(1), Some(x), Some(y)) = (crv, x, y) else { return Err(WaError::UnsupportedKey) };
    let mut point = vec![0x04];
    point.extend_from_slice(x);
    point.extend_from_slice(y);
    p256::ecdsa::VerifyingKey::from_sec1_bytes(&point).map_err(|_| WaError::UnsupportedKey)
}

fn rs256_key(key: &Cbor) -> Result<rsa::RsaPublicKey, WaError> {
    let n = key.get_int(-1).and_then(Cbor::bytes).ok_or(WaError::UnsupportedKey)?;
    let e = key.get_int(-2).and_then(Cbor::bytes).ok_or(WaError::UnsupportedKey)?;
    if n.len() < 256 {
        return Err(WaError::UnsupportedKey); // < 2048 bit
    }
    rsa::RsaPublicKey::new(rsa::BigUint::from_bytes_be(n), rsa::BigUint::from_bytes_be(e)).map_err(|_| WaError::UnsupportedKey)
}

/// `navigator.credentials.get()` result against a stored credential. Returns
/// the new signature counter.
pub fn verify_assertion(
    e: &Expect,
    alg: i64,
    public_key: &str,
    stored_count: u32,
    client_data_json: &[u8],
    authenticator_data: &[u8],
    signature: &[u8],
) -> Result<u32, WaError> {
    check_client_data(e, client_data_json, "webauthn.get")?;
    let ad = auth_data(e, authenticator_data)?;
    let raw = unb64(public_key).ok_or(WaError::UnsupportedKey)?;
    let (key, _) = cbor(&raw)?;
    let mut msg = authenticator_data.to_vec();
    msg.extend_from_slice(&Sha256::digest(client_data_json));
    match alg {
        ES256 => {
            use p256::ecdsa::signature::Verifier;
            let vk = es256_key(&key)?;
            let sig = p256::ecdsa::Signature::from_der(signature).map_err(|_| WaError::BadSignature)?;
            vk.verify(&msg, &sig).map_err(|_| WaError::BadSignature)?;
        }
        RS256 => {
            use rsa::signature::Verifier;
            let vk = rsa::pkcs1v15::VerifyingKey::<Sha256>::new(rs256_key(&key)?);
            let sig = rsa::pkcs1v15::Signature::try_from(signature).map_err(|_| WaError::BadSignature)?;
            vk.verify(&msg, &sig).map_err(|_| WaError::BadSignature)?;
        }
        _ => return Err(WaError::UnsupportedKey),
    }
    if (ad.sign_count != 0 || stored_count != 0) && ad.sign_count <= stored_count {
        return Err(WaError::Replayed);
    }
    Ok(ad.sign_count)
}

#[cfg(test)]
pub(crate) mod testkit {
    //! A software authenticator for tests.
    use super::*;
    use p256::ecdsa::{signature::Signer, SigningKey};

    pub fn enc_head(major: u8, n: u64, out: &mut Vec<u8>) {
        if n < 24 {
            out.push(major << 5 | n as u8);
        } else if n < 256 {
            out.extend([major << 5 | 24, n as u8]);
        } else if n < 65536 {
            out.push(major << 5 | 25);
            out.extend((n as u16).to_be_bytes());
        } else {
            out.push(major << 5 | 26);
            out.extend((n as u32).to_be_bytes());
        }
    }
    pub fn int(i: i64, out: &mut Vec<u8>) {
        if i >= 0 {
            enc_head(0, i as u64, out)
        } else {
            enc_head(1, (-1 - i) as u64, out)
        }
    }
    pub fn bytes(b: &[u8], out: &mut Vec<u8>) {
        enc_head(2, b.len() as u64, out);
        out.extend_from_slice(b);
    }
    pub fn text(t: &str, out: &mut Vec<u8>) {
        enc_head(3, t.len() as u64, out);
        out.extend_from_slice(t.as_bytes());
    }

    pub struct Authenticator {
        pub key: SigningKey,
        pub cred_id: Vec<u8>,
        pub rp_id: String,
        pub origin: String,
        pub count: u32,
        pub flags: u8,
    }

    impl Authenticator {
        pub fn new(seed: u8, rp_id: &str, origin: &str) -> Self {
            let key = SigningKey::from_bytes(&[seed.max(1); 32].into()).unwrap();
            Self { key, cred_id: vec![seed; 16], rp_id: rp_id.into(), origin: origin.into(), count: 0, flags: 0x05 }
        }

        pub fn cose_key(&self) -> Vec<u8> {
            let p = self.key.verifying_key().to_encoded_point(false);
            let mut k = Vec::new();
            enc_head(5, 5, &mut k);
            int(1, &mut k);
            int(2, &mut k);
            int(3, &mut k);
            int(-7, &mut k);
            int(-1, &mut k);
            int(1, &mut k);
            int(-2, &mut k);
            bytes(p.x().unwrap(), &mut k);
            int(-3, &mut k);
            bytes(p.y().unwrap(), &mut k);
            k
        }

        pub fn client_data(&self, kind: &str, challenge: &str) -> Vec<u8> {
            serde_json::json!({"type": kind, "challenge": challenge, "origin": self.origin}).to_string().into_bytes()
        }

        fn auth_data(&self, flags: u8, attested: bool) -> Vec<u8> {
            let mut ad = Sha256::digest(self.rp_id.as_bytes()).to_vec();
            ad.push(flags);
            ad.extend(self.count.to_be_bytes());
            if attested {
                ad.extend([0u8; 16]);
                ad.extend((self.cred_id.len() as u16).to_be_bytes());
                ad.extend(&self.cred_id);
                ad.extend(self.cose_key());
            }
            ad
        }

        /// (clientDataJSON, attestationObject)
        pub fn create(&self, challenge: &str) -> (Vec<u8>, Vec<u8>) {
            let mut att = Vec::new();
            enc_head(5, 3, &mut att);
            text("fmt", &mut att);
            text("none", &mut att);
            text("attStmt", &mut att);
            enc_head(5, 0, &mut att);
            text("authData", &mut att);
            bytes(&self.auth_data(self.flags | 0x40, true), &mut att);
            (self.client_data("webauthn.create", challenge), att)
        }

        /// (clientDataJSON, authenticatorData, signature)
        pub fn get(&mut self, challenge: &str) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
            self.count += 1;
            let cd = self.client_data("webauthn.get", challenge);
            let ad = self.auth_data(self.flags, false);
            let mut msg = ad.clone();
            msg.extend_from_slice(&Sha256::digest(&cd));
            let sig: p256::ecdsa::Signature = self.key.sign(&msg);
            (cd, ad, sig.to_der().as_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testkit::*;
    use super::*;

    const RP: &str = "glyph.example";
    const ORIGIN: &str = "https://glyph.example";

    fn expect<'a>(origins: &'a [String], challenge: &'a str) -> Expect<'a> {
        Expect { rp_id: RP, origins, challenge }
    }

    #[test]
    fn register_then_assert() {
        let origins = vec![ORIGIN.to_string()];
        let mut a = Authenticator::new(7, RP, ORIGIN);
        let (cd, att) = a.create("Y2hhbGxlbmdl");
        let cred = verify_registration(&expect(&origins, "Y2hhbGxlbmdl"), &cd, &att).unwrap();
        assert_eq!(cred.alg, ES256);
        assert_eq!(cred.cred_id, b64(&[7; 16]));

        let (cd, ad, sig) = a.get("bmV4dA");
        let n = verify_assertion(&expect(&origins, "bmV4dA"), cred.alg, &cred.public_key, 0, &cd, &ad, &sig).unwrap();
        assert_eq!(n, 1);
        // Same counter again = cloned authenticator or replay.
        assert_eq!(verify_assertion(&expect(&origins, "bmV4dA"), cred.alg, &cred.public_key, 1, &cd, &ad, &sig), Err(WaError::Replayed));
        // Tampered data.
        let mut bad = ad.clone();
        bad[33] ^= 0xff;
        assert!(verify_assertion(&expect(&origins, "bmV4dA"), cred.alg, &cred.public_key, 0, &cd, &bad, &sig).is_err());
    }

    #[test]
    fn rejects_wrong_context() {
        let origins = vec![ORIGIN.to_string()];
        let a = Authenticator::new(3, RP, ORIGIN);
        let (cd, att) = a.create("abc");
        assert_eq!(verify_registration(&expect(&origins, "abd"), &cd, &att), Err(WaError::WrongChallenge));
        let other = vec!["https://evil.example".to_string()];
        assert_eq!(verify_registration(&expect(&other, "abc"), &cd, &att), Err(WaError::WrongOrigin));
        let wrong_rp = Authenticator::new(3, "evil.example", ORIGIN);
        let (cd, att) = wrong_rp.create("abc");
        assert_eq!(verify_registration(&expect(&origins, "abc"), &cd, &att), Err(WaError::WrongRp));
        let mut no_uv = Authenticator::new(3, RP, ORIGIN);
        no_uv.flags = 0x01;
        let (cd, att) = no_uv.create("abc");
        assert_eq!(verify_registration(&expect(&origins, "abc"), &cd, &att), Err(WaError::NoUserVerification));
        // A get() response is not a create() response.
        let mut a = Authenticator::new(3, RP, ORIGIN);
        let (cd, _, _) = a.get("abc");
        assert_eq!(verify_registration(&expect(&origins, "abc"), &cd, &att), Err(WaError::WrongType));
    }

    #[test]
    fn rs256_assertion() {
        use rsa::pkcs1v15::SigningKey;
        use rsa::signature::{SignatureEncoding, Signer};
        use rsa::traits::PublicKeyParts;
        let mut rng = rand::rngs::StdRng::seed_from_u64(1);
        use rand::SeedableRng;
        let sk = rsa::RsaPrivateKey::new(&mut rng, 2048).unwrap();
        let pk = sk.to_public_key();
        let mut cose = Vec::new();
        enc_head(5, 4, &mut cose);
        int(1, &mut cose);
        int(3, &mut cose);
        int(3, &mut cose);
        int(-257, &mut cose);
        int(-1, &mut cose);
        bytes(&pk.n().to_bytes_be(), &mut cose);
        int(-2, &mut cose);
        bytes(&pk.e().to_bytes_be(), &mut cose);

        let a = Authenticator::new(1, RP, ORIGIN);
        let cd = a.client_data("webauthn.get", "Y2g");
        let mut ad = Sha256::digest(RP.as_bytes()).to_vec();
        ad.extend([0x05, 0, 0, 0, 0]);
        let mut msg = ad.clone();
        msg.extend_from_slice(&Sha256::digest(&cd));
        let sig = SigningKey::<Sha256>::new(sk).sign(&msg).to_vec();
        let origins = vec![ORIGIN.to_string()];
        assert_eq!(verify_assertion(&expect(&origins, "Y2g"), RS256, &b64(&cose), 0, &cd, &ad, &sig), Ok(0));
        let mut bad = sig.clone();
        bad[10] ^= 1;
        assert_eq!(verify_assertion(&expect(&origins, "Y2g"), RS256, &b64(&cose), 0, &cd, &ad, &bad), Err(WaError::BadSignature));
    }

    #[test]
    fn cbor_rejects_garbage() {
        assert!(cbor(&[]).is_err());
        assert!(cbor(&[0x5b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(cbor(&[0x9f]).is_err()); // indefinite array
        let mut deep = vec![0x81; 40];
        deep.push(0x00);
        assert!(cbor(&deep).is_err());
        assert_eq!(cbor(&[0x38, 0x18]).unwrap().0, Cbor::Int(-25));
    }
}
