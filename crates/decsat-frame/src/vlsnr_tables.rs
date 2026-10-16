// made by CasualArclamp
//! VL-SNR header tables, EN 302 307-2 §5.5.2.5 (generated from the
//! standard's text; all 16 sequences agree with gr-dtv's `ph_vlsnr_seq`).
//! Do not edit by hand.

/// The 16 base rows of 56 bits, first transmitted bit in bit 55.
pub const BASE_ROWS: [u64; 16] = [
    0xFBF23E837F9BC4,
    0x98708E0B39345E,
    0xF6A2C9FE1B1737,
    0x8418D95A6F997A,
    0x7B7D7B3E9FC9EA,
    0x5E78BA03A6D51A,
    0x279CC26543ECD0,
    0x342B0498BF3D7D,
    0xADD036E9D5312F,
    0x1061C6DF826237,
    0x72D3E0907384C7,
    0x3BD5ACEE25E2C9,
    0x59087D82615ADA,
    0xE9AF0172CF9DA7,
    0x3F4835A4063F07,
    0x23C9AEECF2ED41,
];

/// Table 18b's Walsh-Hadamard sign patterns by header index: bit `r`
/// (from the MSB, row 0) set means row `r` is sent inverted.
pub const WALSH: [u16; 16] = [
    0b0000000000000000,
    0b0101010101010101,
    0b0011001100110011,
    0b0110011001100110,
    0b0000111100001111,
    0b0110100101101001,
    0b0011001111001100,
    0b0110011010011001,
    0b0000111111110000,
    0b0011110000111100,
    0b0101101001011010,
    0b1111111100000000,
    0b0101010110101010,
    0b0101101010100101,
    0b0011110011000011,
    0b0110100110010110,
];
