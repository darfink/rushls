//! Deterministic pseudo-random input generation for hostile-input tests.
//!
//! The parsers under test are pure and cheap, so a hand-rolled xorshift gives
//! property-style coverage without a property-testing dependency. Seeded
//! explicitly so a failing iteration reproduces exactly.

/// The alphabet hostile inputs are drawn from: the characters these parsers
/// actually care about, plus delimiters and escapes that tend to expose
/// off-by-ones and double-decoding.
const ALPHABET: &[u8] = b"ab/.0123456789%x-m4s_=<>\\?&;:#*~ \t\"'()[]{}";

/// The next xorshift64* value, which is also the next seed.
pub fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

/// One random string of at most `maximum` bytes from [`ALPHABET`].
pub fn string(state: &mut u64, maximum: usize) -> String {
    let length =
        usize::try_from(next_random(state) % (u64::try_from(maximum).unwrap_or(u64::MAX) + 1))
            .unwrap_or(maximum);
    (0..length)
        .map(|_| {
            let index = usize::try_from(next_random(state)).unwrap_or(usize::MAX) % ALPHABET.len();
            ALPHABET[index] as char
        })
        .collect()
}
