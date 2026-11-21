// QRC uses a modified DES S-box. The tables and key order follow the
// lx-music-desktop QRC decoder (Apache-2.0):
// https://github.com/lyswhut/lx-music-desktop/blob/master/src/renderer/utils/musicSdk/tx/qrcDecode.js
use crate::providers::ProviderError;

use super::{MAX_DECODED_BYTES, decode_utf8, inflate_bounded};

const QRC_KEY: [u8; 24] = *b"!@#)(*$%123ZXC!@!@#)(NHL";

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    13, 16, 10, 23, 0, 4, 2, 27, 14, 5, 20, 9, 22, 18, 11, 3, 25, 7, 15, 6, 26, 19, 12, 1, 40, 51,
    30, 36, 46, 54, 29, 39, 50, 44, 32, 47, 43, 48, 38, 55, 33, 52, 45, 41, 49, 35, 28, 31,
];
const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

const P_OUT: [u8; 32] = [
    15, 6, 19, 20, 28, 11, 27, 16, 0, 14, 22, 25, 4, 17, 30, 9, 1, 7, 23, 13, 31, 26, 2, 8, 18, 12,
    29, 5, 21, 10, 3, 24,
];

const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 15, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10,
        10, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

pub(crate) fn decode_qrc(hex: &str) -> Result<String, ProviderError> {
    let encrypted = decode_hex(hex)?;
    if encrypted.is_empty() || encrypted.len() % 8 != 0 {
        return Err(ProviderError::Invalid(
            "QQ Music QRC payload has an invalid length".to_string(),
        ));
    }

    let key1 = &QRC_KEY[0..8];
    let key2 = &QRC_KEY[8..16];
    let key3 = &QRC_KEY[16..24];
    let subkeys1 = key_schedule(key1);
    let subkeys2 = key_schedule(key2);
    let subkeys3 = key_schedule(key3);

    let mut decrypted = encrypted;
    for block in decrypted.as_chunks_mut::<8>().0 {
        let mut value = u64::from_be_bytes(*block);
        value = des_decrypt(value, &subkeys3);
        value = des_encrypt(value, &subkeys2);
        value = des_decrypt(value, &subkeys1);
        *block = value.to_be_bytes();
    }

    decode_utf8(inflate_bounded(&decrypted)?)
}

fn decode_hex(hex: &str) -> Result<Vec<u8>, ProviderError> {
    if !hex.len().is_multiple_of(2) {
        return Err(ProviderError::Invalid(
            "QQ Music QRC payload has an odd hex length".to_string(),
        ));
    }
    if hex.len() / 2 > MAX_DECODED_BYTES {
        return Err(ProviderError::PayloadTooLarge(hex.len() / 2));
    }

    let bytes = hex.as_bytes();
    let mut output = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = hex_digit(pair[0])?;
        let low = hex_digit(pair[1])?;
        output.push((high << 4) | low);
    }
    Ok(output)
}

fn hex_digit(byte: u8) -> Result<u8, ProviderError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ProviderError::Invalid(
            "QQ Music QRC payload contains non-hex data".to_string(),
        )),
    }
}

fn key_schedule(key: &[u8]) -> [u64; 16] {
    let key: [u8; 8] = key.try_into().expect("8-byte DES key");
    let key = u64::from_be_bytes(swap_word_groups(key));
    let selected = permute(key, &PC1, 64);
    let mut left = (((selected >> 28) & 0x0fff_ffff) as u32) << 4;
    let mut right = ((selected & 0x0fff_ffff) as u32) << 4;
    let mut subkeys = [0u64; 16];

    for (index, shift) in SHIFTS.iter().enumerate() {
        left = rotate_key_half(left, *shift);
        right = rotate_key_half(right, *shift);
        let mut subkey = 0u64;
        for (bit_index, position) in PC2.iter().take(24).enumerate() {
            let bit = (left >> (31 - u32::from(*position))) & 1;
            subkey |= (bit as u64) << (47 - bit_index);
        }
        for (bit_index, position) in PC2.iter().skip(24).enumerate() {
            let position = u32::from(*position) - 27;
            let bit = (right >> (31 - position)) & 1;
            subkey |= (bit as u64) << (23 - bit_index);
        }
        subkeys[index] = subkey;
    }

    subkeys
}

fn rotate_key_half(value: u32, shift: u8) -> u32 {
    ((value << shift) | (value >> (28 - shift))) & 0xffff_fff0
}

fn des_encrypt(block: u64, subkeys: &[u64; 16]) -> u64 {
    des_crypt(block, subkeys.iter().copied())
}

fn des_decrypt(block: u64, subkeys: &[u64; 16]) -> u64 {
    des_crypt(block, subkeys.iter().rev().copied())
}

fn des_crypt(block: u64, subkeys: impl Iterator<Item = u64>) -> u64 {
    let block = u64::from_be_bytes(swap_word_groups(block.to_be_bytes()));
    let permuted = permute(block, &IP, 64);
    let mut left = (permuted >> 32) as u32;
    let mut right = permuted as u32;

    for subkey in subkeys {
        let next = left ^ feistel(right, subkey);
        left = right;
        right = next;
    }

    let pre_output = ((right as u64) << 32) | left as u64;
    let output = permute(pre_output, &FP, 64);
    u64::from_be_bytes(swap_word_groups(output.to_be_bytes()))
}

fn swap_word_groups(mut bytes: [u8; 8]) -> [u8; 8] {
    bytes[0..4].reverse();
    bytes[4..8].reverse();
    bytes
}

fn feistel(state: u32, subkey: u64) -> u32 {
    let t1 = bitnum_intl(state, 31, 0)
        | ((state & 0xf000_0000) >> 1)
        | bitnum_intl(state, 4, 5)
        | bitnum_intl(state, 3, 6)
        | ((state & 0x0f00_0000) >> 3)
        | bitnum_intl(state, 8, 11)
        | bitnum_intl(state, 7, 12)
        | ((state & 0x00f0_0000) >> 5)
        | bitnum_intl(state, 12, 17)
        | bitnum_intl(state, 11, 18)
        | ((state & 0x000f_0000) >> 7)
        | bitnum_intl(state, 16, 23);
    let t2 = bitnum_intl(state, 15, 0)
        | ((state & 0x0000_f000) << 15)
        | bitnum_intl(state, 20, 5)
        | bitnum_intl(state, 19, 6)
        | ((state & 0x0000_0f00) << 13)
        | bitnum_intl(state, 24, 11)
        | bitnum_intl(state, 23, 12)
        | ((state & 0x0000_00f0) << 11)
        | bitnum_intl(state, 28, 17)
        | bitnum_intl(state, 27, 18)
        | ((state & 0x0000_000f) << 9)
        | bitnum_intl(state, 0, 23);

    let key = [
        (subkey >> 40) as u8,
        (subkey >> 32) as u8,
        (subkey >> 24) as u8,
        (subkey >> 16) as u8,
        (subkey >> 8) as u8,
        subkey as u8,
    ];
    let mut parts = [
        (t1 >> 24) as u8,
        (t1 >> 16) as u8,
        (t1 >> 8) as u8,
        (t2 >> 24) as u8,
        (t2 >> 16) as u8,
        (t2 >> 8) as u8,
    ];
    for (part, key_byte) in parts.iter_mut().zip(key) {
        *part ^= key_byte;
    }

    let substituted = (u32::from(SBOX[0][sbox_bit(parts[0] >> 2)]) << 28)
        | (u32::from(SBOX[1][sbox_bit(((parts[0] & 0x03) << 4) | (parts[1] >> 4))]) << 24)
        | (u32::from(SBOX[2][sbox_bit(((parts[1] & 0x0f) << 2) | (parts[2] >> 6))]) << 20)
        | (u32::from(SBOX[3][sbox_bit(parts[2] & 0x3f)]) << 16)
        | (u32::from(SBOX[4][sbox_bit(parts[3] >> 2)]) << 12)
        | (u32::from(SBOX[5][sbox_bit(((parts[3] & 0x03) << 4) | (parts[4] >> 4))]) << 8)
        | (u32::from(SBOX[6][sbox_bit(((parts[4] & 0x0f) << 2) | (parts[5] >> 6))]) << 4)
        | u32::from(SBOX[7][sbox_bit(parts[5] & 0x3f)]);

    let mut output = 0u32;
    for (index, position) in P_OUT.iter().enumerate() {
        let bit = (substituted >> (31 - u32::from(*position))) & 1;
        output |= bit << (31 - index);
    }
    output
}

fn bitnum_intl(value: u32, position: u32, shift: u32) -> u32 {
    (value.wrapping_shl(position) & 0x8000_0000) >> shift
}

fn sbox_bit(value: u8) -> usize {
    usize::from((value & 0x20) | ((value & 0x1f) >> 1) | ((value & 0x01) << 4))
}

fn permute(input: u64, table: &[u8], input_bits: u32) -> u64 {
    let mut output = 0u64;
    for &position in table {
        let bit = (input >> (input_bits - position as u32)) & 1;
        output = (output << 1) | bit;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decrypts_qrc_fixture() {
        let hex = "EE7BEFB746F93831D42DCDF2E21F0D5D977DE00C85449A7C535DA3BCF1BEB3CA54D5D801D40005B7819BEA94D6A3734A54A024D295630DCE5F98584248D26B4046E23B9826994A79ED9023F0130273603388036481F618FB65FA46AF184DF6DE";
        let text = decode_qrc(hex).unwrap();
        assert!(text.contains("Look &amp;"));
        assert!(text.contains("shine"));
    }

    #[test]
    fn rejects_odd_hex() {
        assert!(matches!(decode_qrc("ABC"), Err(ProviderError::Invalid(_))));
    }

    #[test]
    fn rejects_oversized_input() {
        let hex = "00".repeat(MAX_DECODED_BYTES + 1);
        assert!(matches!(
            decode_qrc(&hex),
            Err(ProviderError::PayloadTooLarge(_))
        ));
    }
}
