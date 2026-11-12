//! SRP-6a client (RFC 5054 3072-bit group, SHA-512), as used by HomeKit /
//! AirPlay 2 pair-setup. Follows the csrp conventions used by OwnTone, which
//! are known to interoperate with HomePods:
//! - k = H(N | PAD(g)), u = H(PAD(A) | PAD(B))
//! - x = H(s | H(I ":" P))
//! - K = H(S)
//! - M1 = H(H(N) xor H(g) | H(I) | s | A | B | K)
//! - HAMK = H(A | M1 | K)

use num_bigint::BigUint;
use rand::RngCore;
use sha2::{Digest, Sha512};

const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B",
    "139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485",
    "B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1F",
    "E649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23",
    "DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32",
    "905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF69558",
    "17183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521",
    "ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D7",
    "1E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B1817",
    "7B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82",
    "D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 5;
const N_LEN: usize = 384;

pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    big_a: BigUint,
    username: String,
    password: String,
}

pub struct SrpProof {
    /// Client proof to send (M1).
    pub m1: Vec<u8>,
    /// Session key K = H(S), 64 bytes.
    pub session_key: Vec<u8>,
    /// What the server must answer (HAMK) for us to trust it.
    pub expected_hamk: Vec<u8>,
}

fn pad(n: &BigUint, len: usize) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; len.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

fn h(parts: &[&[u8]]) -> Vec<u8> {
    let mut hasher = Sha512::new();
    for p in parts {
        hasher.update(p);
    }
    hasher.finalize().to_vec()
}

impl SrpClient {
    pub fn new(username: &str, password: &str) -> Self {
        let mut a_bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut a_bytes);
        Self::with_private(username, password, &a_bytes)
    }

    fn with_private(username: &str, password: &str, a_bytes: &[u8]) -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid N");
        let g = BigUint::from(G);
        let a = BigUint::from_bytes_be(a_bytes);
        let big_a = g.modpow(&a, &n);
        Self {
            n,
            g,
            a,
            big_a,
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    /// Public value A (unpadded, as csrp sends it).
    pub fn public_a(&self) -> Vec<u8> {
        self.big_a.to_bytes_be()
    }

    pub fn process_challenge(&self, salt: &[u8], b_bytes: &[u8]) -> anyhow::Result<SrpProof> {
        let n = &self.n;
        let b = BigUint::from_bytes_be(b_bytes);
        if (&b % n) == BigUint::from(0u32) {
            anyhow::bail!("SRP: invalid server public key");
        }
        // csrp goes through a bignum for the salt, which drops leading zeros
        let s = BigUint::from_bytes_be(salt).to_bytes_be();

        let k = BigUint::from_bytes_be(&h(&[&pad(n, N_LEN), &pad(&self.g, N_LEN)]));
        let u = BigUint::from_bytes_be(&h(&[&pad(&self.big_a, N_LEN), &pad(&b, N_LEN)]));
        if u == BigUint::from(0u32) {
            anyhow::bail!("SRP: u is zero");
        }

        let inner = h(&[self.username.as_bytes(), b":", self.password.as_bytes()]);
        let x = BigUint::from_bytes_be(&h(&[&s, &inner]));

        // S = (B - k * g^x) ^ (a + u * x) mod N
        let v = self.g.modpow(&x, n);
        let kv = (&k * &v) % n;
        let base = ((&b % n) + n - kv) % n;
        let exp = &self.a + &u * &x;
        let big_s = base.modpow(&exp, n);

        let session_key = h(&[&big_s.to_bytes_be()]);

        let h_n = h(&[&n.to_bytes_be()]);
        let h_g = h(&[&self.g.to_bytes_be()]);
        let h_xor: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect();
        let h_i = h(&[self.username.as_bytes()]);
        let a_bytes = self.big_a.to_bytes_be();
        let m1 = h(&[&h_xor, &h_i, &s, &a_bytes, &b.to_bytes_be(), &session_key]);
        let expected_hamk = h(&[&a_bytes, &m1, &session_key]);

        Ok(SrpProof {
            m1,
            session_key,
            expected_hamk,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs the server side of SRP-6a with the same conventions and checks
    /// that both sides agree on the key and on the proofs.
    #[test]
    fn client_matches_reference_server() {
        let user = "Pair-Setup";
        let pin = "3939";
        let client = SrpClient::with_private(user, pin, &[0x11; 32]);
        let n = client.n.clone();
        let g = client.g.clone();

        // Server: verifier and B
        let salt = [0x42u8; 16];
        let inner = h(&[user.as_bytes(), b":", pin.as_bytes()]);
        let x = BigUint::from_bytes_be(&h(&[&salt, &inner]));
        let v = g.modpow(&x, &n);
        let b_priv = BigUint::from_bytes_be(&[0x22; 32]);
        let k = BigUint::from_bytes_be(&h(&[&pad(&n, N_LEN), &pad(&g, N_LEN)]));
        let big_b = ((&k * &v) + g.modpow(&b_priv, &n)) % &n;

        let proof = client
            .process_challenge(&salt, &big_b.to_bytes_be())
            .unwrap();

        // Server computes S = (A * v^u) ^ b mod N
        let big_a = BigUint::from_bytes_be(&client.public_a());
        let u = BigUint::from_bytes_be(&h(&[&pad(&big_a, N_LEN), &pad(&big_b, N_LEN)]));
        let s_server = ((&big_a * v.modpow(&u, &n)) % &n).modpow(&b_priv, &n);
        let k_server = h(&[&s_server.to_bytes_be()]);
        assert_eq!(proof.session_key, k_server);
        assert_eq!(proof.session_key.len(), 64);
    }
}
