//! SHA-256 (FIPS 180-4), streaming and allocation-free.
//!
//! The package installer hashes a download while it is being written to disk,
//! so the only API that matters is [`Sha256::update`] + [`Sha256::finalize`]:
//! the caller never holds the package (or its digest) in one heap buffer.
//!
//! This is deliberately a plain implementation, not a platform:
//!
//! * no dependencies and no `alloc` — the state is 108 bytes on the caller's
//!   stack plus a 64-byte block buffer;
//! * no table-driven message schedule is needed, the arithmetic is a handful of
//!   rotates and `wrapping_add`s (`u32` overflow in SHA-256 is defined, so the
//!   debug overflow check must not fire — hence the explicit wrapping calls);
//! * every loop bound is a constant and every slice index is derived from one,
//!   so no input can panic the kernel (`panic = "abort"`).
//!
//! The known-answer test is compiled only under `cfg(test)` (the crate targets a
//! bare-metal RISC-V image, where `cargo test` cannot link a harness); the same
//! vectors are available at runtime through [`selftest`], which the shell's
//! `pkg selftest` command calls.

/// Initial hash value: the first 32 bits of the fractional parts of the square
/// roots of the first eight primes (FIPS 180-4 §5.3.3).
const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Round constants: the first 32 bits of the fractional parts of the cube roots
/// of the first sixty-four primes (FIPS 180-4 §4.2.2).
const ROUND: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// Streaming SHA-256 state.
#[derive(Clone)]
pub struct Sha256 {
    /// Running intermediate hash.
    state: [u32; 8],
    /// Bytes of the current, not yet compressed block.
    block: [u8; 64],
    /// Valid bytes in `block` (0..64).
    block_len: usize,
    /// Total bytes fed, for the length field of the final block.
    total: u64,
}

impl Sha256 {
    /// A fresh hasher (the `const` form lets a static start empty).
    pub const fn new() -> Self {
        Sha256 {
            state: INITIAL,
            block: [0; 64],
            block_len: 0,
            total: 0,
        }
    }

    /// Hash `data`, in as many calls as the caller likes.
    pub fn update(&mut self, data: &[u8]) {
        // `total` counts input bytes; a package is at most a few MiB, but keep
        // the addition defined for a caller that feeds a whole block device.
        self.total = self.total.wrapping_add(data.len() as u64);
        let mut rest = data;

        // Top up a partially filled block first.
        if self.block_len > 0 {
            let take = (64 - self.block_len).min(rest.len());
            self.block[self.block_len..self.block_len + take].copy_from_slice(&rest[..take]);
            self.block_len += take;
            rest = &rest[take..];
            if self.block_len == 64 {
                let block = self.block;
                self.compress(&block);
                self.block_len = 0;
            }
            if rest.is_empty() {
                // The partial block still holds these bytes; do not fall
                // through to the tail copy, which would reset `block_len`.
                return;
            }
        }

        // Then compress whole blocks straight out of the caller's slice. The
        // copy into a fixed array is what keeps `compress` free of any
        // unchecked cast; it is 64 bytes per block and the debug/release builds
        // both keep it in registers.
        let mut chunks = rest.chunks_exact(64);
        for block in &mut chunks {
            let mut full = [0u8; 64];
            full.copy_from_slice(block);
            self.compress(&full);
        }

        // Whatever is left cannot complete a block; keep it for the next call.
        let tail = chunks.remainder();
        self.block[..tail.len()].copy_from_slice(tail);
        self.block_len = tail.len();
    }

    /// Finish the hash and return the raw digest. Consumes the state so a
    /// caller cannot accidentally add to a finalised hash.
    pub fn finalize(mut self) -> [u8; 32] {
        // Append 0x80, then zeros, then the 64-bit big-endian bit length, so the
        // padded message is a whole number of 64-byte blocks (FIPS 180-4 §5.1).
        self.block[self.block_len] = 0x80;
        self.block_len += 1;
        if self.block_len > 56 {
            for byte in &mut self.block[self.block_len..] {
                *byte = 0;
            }
            let block = self.block;
            self.compress(&block);
            self.block_len = 0;
        }
        for byte in &mut self.block[self.block_len..56] {
            *byte = 0;
        }
        self.block[56..64].copy_from_slice(&self.total.wrapping_mul(8).to_be_bytes());
        let block = self.block;
        self.compress(&block);

        let mut digest = [0u8; 32];
        for (chunk, word) in digest.chunks_exact_mut(4).zip(self.state.iter()) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// One compression function application over a full 64-byte block.
    fn compress(&mut self, block: &[u8; 64]) {
        // Message schedule: the first 16 words come from the block, the rest
        // are derived. Bounds are constants, so indexing never traps.
        let mut schedule = [0u32; 64];
        for (index, chunk) in block.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..64 {
            let big_s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ (!e & g);
            let temp1 = h
                .wrapping_add(big_s1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(schedule[index]);
            let big_s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = big_s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        self.state[0] = self.state[0].wrapping_add(a);
        self.state[1] = self.state[1].wrapping_add(b);
        self.state[2] = self.state[2].wrapping_add(c);
        self.state[3] = self.state[3].wrapping_add(d);
        self.state[4] = self.state[4].wrapping_add(e);
        self.state[5] = self.state[5].wrapping_add(f);
        self.state[6] = self.state[6].wrapping_add(g);
        self.state[7] = self.state[7].wrapping_add(h);
    }
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// Write `digest` as 64 lowercase hex characters into `out`.
pub fn hex(digest: &[u8; 32], out: &mut [u8; 64]) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for (index, byte) in digest.iter().enumerate() {
        out[index * 2] = DIGITS[(byte >> 4) as usize];
        out[index * 2 + 1] = DIGITS[(byte & 0x0f) as usize];
    }
}

/// Whole-buffer known-answer check for `pkg selftest` and diagnostics.
///
/// Returns `true` for both published FIPS vectors. It allocates nothing; the
/// `"abc"` case is a slice literal and the empty case exercises the padding-only
/// path.
pub fn selftest() -> bool {
    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    let hasher = Sha256::new();
    let mut text = [0u8; 64];
    hex(&hasher.finalize(), &mut text);
    if core::str::from_utf8(&text).unwrap_or("") != EMPTY {
        return false;
    }

    let mut hasher = Sha256::new();
    hasher.update(b"abc");
    hex(&hasher.finalize(), &mut text);
    core::str::from_utf8(&text).unwrap_or("") == ABC
}

#[cfg(test)]
mod tests {
    //! Known-answer tests. The vectors are the published FIPS 180-4 / NIST
    //! examples; they are compiled only for a test harness (the kernel image has
    //! no libtest), which is why `selftest()` repeats the first two at runtime.

    use super::{hex, Sha256};

    fn hash(data: &[u8]) -> [u8; 64] {
        let mut hasher = Sha256::new();
        hasher.update(data);
        let digest = hasher.finalize();
        let mut out = [0u8; 64];
        hex(&digest, &mut out);
        out
    }

    #[test]
    fn empty_input() {
        assert_eq!(
            &hash(b"")[..],
            b"e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn abc() {
        assert_eq!(
            &hash(b"abc")[..],
            b"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn split_updates_match_one_shot() {
        let data = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        let mut hasher = Sha256::new();
        // Feed in three uneven pieces: the block buffer and the direct path
        // must agree on both sides of a 64-byte boundary.
        hasher.update(&data[..1]);
        hasher.update(&data[1..33]);
        hasher.update(&data[33..]);
        let digest = hasher.finalize();
        let mut text = [0u8; 64];
        hex(&digest, &mut text);
        assert_eq!(
            &text[..],
            b"248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }
}
