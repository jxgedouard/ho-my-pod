//! Minimal ALAC "encoder": emits uncompressed (escape) frames. That is valid
//! ALAC that every decoder accepts, costs no CPU and adds no latency, and at
//! ~1.4 Mbit/s it is nothing for a LAN.
//!
//! Frame layout (bit-packed, MSB first):
//!   element type   3  (1 = channel pair element, stereo)
//!   element tag    4  (0)
//!   unused        12  (0)
//!   has size       1  (1 -> a 32-bit sample count follows)
//!   bytes shifted  2  (0)
//!   escape         1  (1 -> samples are stored verbatim)
//!   sample count  32
//!   samples       16 bits each, interleaved L R L R ...
//!   end tag        3  (7)

struct BitWriter {
    buf: Vec<u8>,
    bitpos: usize,
}

impl BitWriter {
    fn with_capacity(bytes: usize) -> Self {
        Self {
            buf: Vec::with_capacity(bytes),
            bitpos: 0,
        }
    }

    fn write(&mut self, value: u32, bits: u32) {
        for i in (0..bits).rev() {
            let bit = (value >> i) & 1;
            let byte = self.bitpos / 8;
            if byte == self.buf.len() {
                self.buf.push(0);
            }
            if bit == 1 {
                self.buf[byte] |= 0x80 >> (self.bitpos % 8);
            }
            self.bitpos += 1;
        }
    }

    fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// `pcm` holds interleaved stereo i16 samples.
pub fn encode_uncompressed(pcm: &[i16]) -> Vec<u8> {
    let frames = (pcm.len() / 2) as u32;
    let mut w = BitWriter::with_capacity(pcm.len() * 2 + 16);
    w.write(1, 3); // CPE
    w.write(0, 4);
    w.write(0, 12);
    w.write(1, 1); // has size
    w.write(0, 2);
    w.write(1, 1); // escape: uncompressed
    w.write(frames, 32);
    for &s in pcm {
        w.write(s as u16 as u32, 16);
    }
    w.write(7, 3); // ID_END
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_bits_match_reference() {
        // Reference: libraop's pcm_to_alac_raw writes 0x20 0x00 0x12 then the count
        let out = encode_uncompressed(&[0i16; 352 * 2]);
        assert_eq!(out[0], 0x20);
        assert_eq!(out[1], 0x00);
        assert_eq!(out[2], 0x12);
        // 23 + 32 + 352*32 + 3 bits = 11322 bits -> 1416 bytes
        assert_eq!(out.len(), 1416);
    }

    #[test]
    fn samples_are_shifted_by_55_bits() {
        let out = encode_uncompressed(&[0x7FFF, -1]);
        // sample count = 1, following the 23 header bits
        // first sample starts at bit 55: byte 6, bit 7
        let mut bits = Vec::new();
        for b in &out {
            for i in (0..8).rev() {
                bits.push((b >> i) & 1);
            }
        }
        let read = |start: usize, n: usize| -> u32 {
            bits[start..start + n].iter().fold(0u32, |acc, &b| (acc << 1) | b as u32)
        };
        assert_eq!(read(23, 32), 1);
        assert_eq!(read(55, 16), 0x7FFF);
        assert_eq!(read(71, 16), 0xFFFF);
        assert_eq!(read(87, 3), 7);
    }
}
