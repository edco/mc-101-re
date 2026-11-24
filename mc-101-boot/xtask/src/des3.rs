//! Pure-Rust FIPS 46-3 DES / 3DES-ECB implementation for Roland MC-101 `RPG69_C0A_up.bin`.
//!
//! Matches the hardware DES/3DES peripheral (`0x4002_0000`) AXI 32-bit word-pair swap
//! and BootROM key table (`0x0120_23B0 + key_idx * 24`) verified in `tools/decrypt_c0a.py`.

#![allow(clippy::needless_range_loop)]

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4,
    62, 54, 46, 38, 30, 22, 14, 6, 64, 56, 48, 40, 32, 24, 16, 8,
    57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3,
    61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];

const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31,
    38, 6, 46, 14, 54, 22, 62, 30, 37, 5, 45, 13, 53, 21, 61, 29,
    36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];

const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11,
    12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18, 19, 20, 21, 20, 21,
    22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];

const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10,
    2, 8, 24, 14, 32, 27, 3, 9, 19, 13, 30, 6, 22, 11, 4, 25,
];

const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18,
    10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60, 52, 44, 36,
    63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22,
    14, 6, 61, 53, 45, 37, 29, 21, 13, 5, 28, 20, 12, 4,
];

const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4,
    26, 8, 16, 7, 27, 20, 13, 2, 41, 52, 31, 37, 47, 55, 30, 40,
    51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];

const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

const SBOX: [[[u8; 16]; 4]; 8] = [
    [
        [14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7],
        [0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8],
        [4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0],
        [15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13],
    ],
    [
        [15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10],
        [3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5],
        [0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15],
        [13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9],
    ],
    [
        [10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8],
        [13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1],
        [13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7],
        [1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12],
    ],
    [
        [7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15],
        [13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9],
        [10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4],
        [3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14],
    ],
    [
        [2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9],
        [14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6],
        [4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14],
        [11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3],
    ],
    [
        [12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11],
        [10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8],
        [9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6],
        [4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13],
    ],
    [
        [4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1],
        [13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6],
        [1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2],
        [6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12],
    ],
    [
        [13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7],
        [1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2],
        [7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8],
        [2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11],
    ],
];

fn permute64(val: u64, table: &[u8]) -> u64 {
    let mut out = 0u64;
    let n = table.len();
    for (i, &pos) in table.iter().enumerate() {
        let bit = (val >> (64 - pos as usize)) & 1;
        out |= bit << (n - 1 - i);
    }
    out
}

fn permute56(val: u64, table: &[u8]) -> u64 {
    let mut out = 0u64;
    let n = table.len();
    for (i, &pos) in table.iter().enumerate() {
        let bit = (val >> (56 - pos as usize)) & 1;
        out |= bit << (n - 1 - i);
    }
    out
}

fn permute32(val: u32, table: &[u8]) -> u32 {
    let mut out = 0u32;
    let n = table.len();
    for (i, &pos) in table.iter().enumerate() {
        let bit = (val >> (32 - pos as usize)) & 1;
        out |= bit << (n - 1 - i);
    }
    out
}

fn des_subkeys(key8: &[u8]) -> [u64; 16] {
    let k64 = u64::from_be_bytes(key8[..8].try_into().unwrap());
    let k56 = permute64(k64, &PC1);
    let mut c = ((k56 >> 28) & 0x0FFF_FFFF) as u32;
    let mut d = (k56 & 0x0FFF_FFFF) as u32;
    let mut sub = [0u64; 16];
    for i in 0..16 {
        let sh = SHIFTS[i];
        c = ((c << sh) | (c >> (28 - sh))) & 0x0FFF_FFFF;
        d = ((d << sh) | (d >> (28 - sh))) & 0x0FFF_FFFF;
        let cd = ((c as u64) << 28) | (d as u64);
        sub[i] = permute56(cd, &PC2);
    }
    sub
}

fn des_feistel(r: u32, sk: u64) -> u32 {
    let mut er = 0u64;
    for (i, &pos) in E.iter().enumerate() {
        let bit = ((r >> (32 - pos as usize)) & 1) as u64;
        er |= bit << (47 - i);
    }
    let x = er ^ sk;
    let mut s_out = 0u32;
    for box_i in 0..8 {
        let chunk = ((x >> (42 - box_i * 6)) & 0x3F) as usize;
        let row = ((chunk & 0x20) >> 4) | (chunk & 1);
        let col = (chunk >> 1) & 0x0F;
        let v = SBOX[box_i][row][col] as u32;
        s_out = (s_out << 4) | v;
    }
    permute32(s_out, &P)
}

fn des_block(block: u64, sub: &[u64; 16], encrypt: bool) -> u64 {
    let p = permute64(block, &IP);
    let mut l = (p >> 32) as u32;
    let mut r = p as u32;
    for i in 0..16 {
        let sk = if encrypt { sub[i] } else { sub[15 - i] };
        let next_r = l ^ des_feistel(r, sk);
        l = r;
        r = next_r;
    }
    let rl = ((r as u64) << 32) | (l as u64);
    permute64(rl, &FP)
}

pub struct Mc101Des3 {
    sk1: [u64; 16],
    sk2: [u64; 16],
    sk3: [u64; 16],
}

impl Mc101Des3 {
    /// Derive key from Silicon Boot ROM dump (`bootrom_full_16k.bin`, slot `0xB6` at `0x34C0`).
    pub fn from_bootrom(rom: &[u8], key_idx: usize) -> Result<Self, String> {
        let off = 0x23B0 + key_idx * 24;
        if off + 24 > rom.len() {
            return Err("BootROM too small for key slot".into());
        }
        let mut w = [0u32; 6];
        for i in 0..6 {
            w[i] = u32::from_le_bytes(rom[off + i * 4..off + i * 4 + 4].try_into().unwrap());
        }
        // Pack as >IIIIII (w1, w0, w3, w2, w5, w4) matching tools/decrypt_c0a.py
        let mut key24 = [0u8; 24];
        key24[0..4].copy_from_slice(&w[1].to_be_bytes());
        key24[4..8].copy_from_slice(&w[0].to_be_bytes());
        key24[8..12].copy_from_slice(&w[3].to_be_bytes());
        key24[12..16].copy_from_slice(&w[2].to_be_bytes());
        key24[16..20].copy_from_slice(&w[5].to_be_bytes());
        key24[20..24].copy_from_slice(&w[4].to_be_bytes());

        Ok(Self {
            sk1: des_subkeys(&key24[0..8]),
            sk2: des_subkeys(&key24[8..16]),
            sk3: des_subkeys(&key24[16..24]),
        })
    }

    pub fn encrypt_ecb(&self, plaintext: &[u8]) -> Vec<u8> {
        assert_eq!(plaintext.len() % 8, 0);
        let mut out = vec![0u8; plaintext.len()];
        for i in (0..plaintext.len()).step_by(8) {
            let w0 = u32::from_le_bytes(plaintext[i..i + 4].try_into().unwrap());
            let w1 = u32::from_le_bytes(plaintext[i + 4..i + 8].try_into().unwrap());
            let be_in = ((w1 as u64) << 32) | (w0 as u64);
            let b1 = des_block(be_in, &self.sk1, true);
            let b2 = des_block(b1, &self.sk2, false);
            let be_out = des_block(b2, &self.sk3, true);
            let o1 = (be_out >> 32) as u32;
            let o0 = be_out as u32;
            out[i..i + 4].copy_from_slice(&o0.to_le_bytes());
            out[i + 4..i + 8].copy_from_slice(&o1.to_le_bytes());
        }
        out
    }

    pub fn decrypt_ecb(&self, ciphertext: &[u8]) -> Vec<u8> {
        assert_eq!(ciphertext.len() % 8, 0);
        let mut out = vec![0u8; ciphertext.len()];
        for i in (0..ciphertext.len()).step_by(8) {
            let w0 = u32::from_le_bytes(ciphertext[i..i + 4].try_into().unwrap());
            let w1 = u32::from_le_bytes(ciphertext[i + 4..i + 8].try_into().unwrap());
            let be_in = ((w1 as u64) << 32) | (w0 as u64);
            let b1 = des_block(be_in, &self.sk3, false);
            let b2 = des_block(b1, &self.sk2, true);
            let be_out = des_block(b2, &self.sk1, false);
            let o1 = (be_out >> 32) as u32;
            let o0 = be_out as u32;
            out[i..i + 4].copy_from_slice(&o0.to_le_bytes());
            out[i + 4..i + 8].copy_from_slice(&o1.to_le_bytes());
        }
        out
    }
}
