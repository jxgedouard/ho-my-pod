//! Encryption of the RTSP control/event channels after pairing, and of the
//! RTP audio packets.

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};
use hkdf::Hkdf;
use sha2::Sha512;

const BLOCK_MAX: usize = 0x400;
const TAG_LEN: usize = 16;

pub fn hkdf_sha512(ikm: &[u8], salt: &str, info: &str) -> [u8; 32] {
    let hk = Hkdf::<Sha512>::new(Some(salt.as_bytes()), ikm);
    let mut okm = [0u8; 32];
    hk.expand(info.as_bytes(), &mut okm)
        .expect("32 bytes is a valid HKDF-SHA512 length");
    okm
}

fn counter_nonce(counter: u64) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(&counter.to_le_bytes());
    *Nonce::from_slice(&n)
}

/// Framed ChaCha20-Poly1305 used on the RTSP connection once paired:
/// each block is `len (u16 LE, also the AAD) | ciphertext | tag`, with a
/// per-direction 64-bit counter as nonce.
pub struct FramedCipher {
    enc: ChaCha20Poly1305,
    dec: ChaCha20Poly1305,
    enc_counter: u64,
    dec_counter: u64,
}

impl FramedCipher {
    pub fn new(enc_key: [u8; 32], dec_key: [u8; 32]) -> Self {
        Self {
            enc: ChaCha20Poly1305::new(Key::from_slice(&enc_key)),
            dec: ChaCha20Poly1305::new(Key::from_slice(&dec_key)),
            enc_counter: 0,
            dec_counter: 0,
        }
    }

    /// Control channel, as seen by the sender (client).
    pub fn control(shared_secret: &[u8]) -> Self {
        Self::new(
            hkdf_sha512(shared_secret, "Control-Salt", "Control-Write-Encryption-Key"),
            hkdf_sha512(shared_secret, "Control-Salt", "Control-Read-Encryption-Key"),
        )
    }

    /// Event channel: a reverse connection, so the key roles are swapped.
    pub fn events(shared_secret: &[u8]) -> Self {
        Self::new(
            hkdf_sha512(shared_secret, "Events-Salt", "Events-Read-Encryption-Key"),
            hkdf_sha512(shared_secret, "Events-Salt", "Events-Write-Encryption-Key"),
        )
    }

    pub fn encrypt(&mut self, plain: &[u8]) -> anyhow::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(plain.len() + (plain.len() / BLOCK_MAX + 1) * 18);
        for block in plain.chunks(BLOCK_MAX) {
            let len = (block.len() as u16).to_le_bytes();
            let mut buf = block.to_vec();
            let tag = self
                .enc
                .encrypt_in_place_detached(&counter_nonce(self.enc_counter), &len, &mut buf)
                .map_err(|_| anyhow::anyhow!("encryption failed"))?;
            self.enc_counter += 1;
            out.extend_from_slice(&len);
            out.extend_from_slice(&buf);
            out.extend_from_slice(&tag);
        }
        Ok(out)
    }

    /// Decrypts every complete block at the start of `input`, removes them
    /// from it, and appends the plaintext to `output`.
    pub fn decrypt_available(&mut self, input: &mut Vec<u8>, output: &mut Vec<u8>) -> anyhow::Result<()> {
        let mut pos = 0;
        while input.len() - pos >= 2 {
            let len = u16::from_le_bytes([input[pos], input[pos + 1]]) as usize;
            let total = 2 + len + TAG_LEN;
            if input.len() - pos < total {
                break;
            }
            let aad = [input[pos], input[pos + 1]];
            let mut buf = input[pos + 2..pos + 2 + len].to_vec();
            let tag = Tag::clone_from_slice(&input[pos + 2 + len..pos + total]);
            self.dec
                .decrypt_in_place_detached(&counter_nonce(self.dec_counter), &aad, &mut buf, &tag)
                .map_err(|_| anyhow::anyhow!("decryption failed (bad key or corrupted stream)"))?;
            self.dec_counter += 1;
            output.extend_from_slice(&buf);
            pos += total;
        }
        input.drain(..pos);
        Ok(())
    }
}

/// Encryption of RTP audio packets with the stream key (`shk`).
pub struct PacketCipher {
    aead: ChaCha20Poly1305,
}

impl PacketCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    /// `rtp` is a 12-byte RTP header followed by the payload. Returns
    /// `header | ciphertext | tag | nonce[4..12]`. The nonce is derived from
    /// the sequence number, so a retransmitted packet is byte-identical.
    pub fn seal(&self, rtp: &[u8]) -> Vec<u8> {
        let header = &rtp[..12];
        let seq = [rtp[2], rtp[3]];
        let mut nonce = [0u8; 12];
        // seqnum copied in host (little-endian) order, as OwnTone does
        nonce[4] = seq[1];
        nonce[5] = seq[0];
        let mut payload = rtp[12..].to_vec();
        let tag = self
            .aead
            .encrypt_in_place_detached(Nonce::from_slice(&nonce), &header[4..12], &mut payload)
            .expect("chacha20poly1305 encryption cannot fail for small buffers");
        let mut out = Vec::with_capacity(rtp.len() + TAG_LEN + 8);
        out.extend_from_slice(header);
        out.extend_from_slice(&payload);
        out.extend_from_slice(&tag);
        out.extend_from_slice(&nonce[4..]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn framed_roundtrip_multi_block() {
        let secret = [9u8; 64];
        let mut client = FramedCipher::control(&secret);
        // The receiver's view: it encrypts with Read and decrypts with Write
        let mut server = FramedCipher::new(
            hkdf_sha512(&secret, "Control-Salt", "Control-Read-Encryption-Key"),
            hkdf_sha512(&secret, "Control-Salt", "Control-Write-Encryption-Key"),
        );
        let msg: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
        let mut wire = client.encrypt(&msg).unwrap();
        // Feed it in two pieces to exercise partial blocks
        let tail = wire.split_off(1500);
        let mut out = Vec::new();
        server.decrypt_available(&mut wire, &mut out).unwrap();
        wire.extend_from_slice(&tail);
        server.decrypt_available(&mut wire, &mut out).unwrap();
        assert!(wire.is_empty());
        assert_eq!(out, msg);

        let reply = server.encrypt(b"RTSP/1.0 200 OK\r\n\r\n").unwrap();
        let mut reply = reply;
        let mut plain = Vec::new();
        client.decrypt_available(&mut reply, &mut plain).unwrap();
        assert_eq!(plain, b"RTSP/1.0 200 OK\r\n\r\n");
    }
}
