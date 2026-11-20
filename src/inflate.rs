//! Gzip and raw DEFLATE decoding (RFC 1952 and 1951), for terrain tiles that are
//! published gzipped. Small and slow-ish, which is fine for a few tiles a year.

use anyhow::{Result, bail};

struct Bits<'a> {
    data: &'a [u8],
    pos: usize,
    bit: u32,
}

impl Bits<'_> {
    fn get(&mut self, n: u32) -> Result<u32> {
        let mut v = 0;
        for i in 0..n {
            let Some(byte) = self.data.get(self.pos) else {
                bail!("compressed data ends early");
            };
            v |= (((byte >> self.bit) & 1) as u32) << i;
            self.bit += 1;
            if self.bit == 8 {
                self.bit = 0;
                self.pos += 1;
            }
        }
        Ok(v)
    }

    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }
}

/// A canonical Huffman code: how many codes of each length, and the symbols in order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0u16; 16];
        for &l in lengths {
            counts[l as usize] += 1;
        }
        counts[0] = 0;
        let mut offs = [0u16; 16];
        for i in 1..16 {
            offs[i] = offs[i - 1] + counts[i - 1];
        }
        let mut symbols = vec![0u16; lengths.len()];
        for (sym, &l) in lengths.iter().enumerate() {
            if l != 0 {
                symbols[offs[l as usize] as usize] = sym as u16;
                offs[l as usize] += 1;
            }
        }
        Self { counts, symbols }
    }

    fn decode(&self, b: &mut Bits) -> Result<u16> {
        let (mut code, mut first, mut index) = (0i32, 0i32, 0i32);
        for len in 1..16 {
            code |= b.get(1)? as i32;
            let count = self.counts[len] as i32;
            if code - count < first {
                return Ok(self.symbols[(index + (code - first)) as usize]);
            }
            index += count;
            first += count;
            first <<= 1;
            code <<= 1;
        }
        bail!("bad Huffman code in compressed data")
    }
}

const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

fn codes(b: &mut Bits, out: &mut Vec<u8>, lit: &Huffman, dist: &Huffman, max: usize) -> Result<()> {
    loop {
        let sym = lit.decode(b)? as usize;
        match sym {
            0..=255 => out.push(sym as u8),
            256 => return Ok(()),
            257..=285 => {
                let i = sym - 257;
                let len = LEN_BASE[i] as usize + b.get(LEN_EXTRA[i] as u32)? as usize;
                let d = dist.decode(b)? as usize;
                if d >= 30 {
                    bail!("bad distance code in compressed data");
                }
                let back = DIST_BASE[d] as usize + b.get(DIST_EXTRA[d] as u32)? as usize;
                if back > out.len() {
                    bail!("compressed data refers back before its start");
                }
                let start = out.len() - back;
                for k in 0..len {
                    out.push(out[start + k]);
                }
            }
            _ => bail!("bad length code in compressed data"),
        }
        if out.len() > max {
            bail!("decompressed data is larger than {max} bytes");
        }
    }
}

fn fixed() -> (Huffman, Huffman) {
    let mut l = [0u8; 288];
    l[..144].fill(8);
    l[144..256].fill(9);
    l[256..280].fill(7);
    l[280..].fill(8);
    (Huffman::new(&l), Huffman::new(&[5u8; 30]))
}

fn dynamic(b: &mut Bits) -> Result<(Huffman, Huffman)> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let nlen = b.get(5)? as usize + 257;
    let ndist = b.get(5)? as usize + 1;
    let ncode = b.get(4)? as usize + 4;
    let mut cl = [0u8; 19];
    for &o in &ORDER[..ncode] {
        cl[o] = b.get(3)? as u8;
    }
    let clh = Huffman::new(&cl);
    let mut lengths = vec![0u8; nlen + ndist];
    let mut i = 0;
    while i < nlen + ndist {
        let sym = clh.decode(b)?;
        let (val, rep) = match sym {
            0..=15 => (sym as u8, 1),
            16 => {
                if i == 0 {
                    bail!("repeat with no previous length in compressed data");
                }
                (lengths[i - 1], 3 + b.get(2)? as usize)
            }
            17 => (0, 3 + b.get(3)? as usize),
            _ => (0, 11 + b.get(7)? as usize),
        };
        if i + rep > nlen + ndist {
            bail!("too many code lengths in compressed data");
        }
        lengths[i..i + rep].fill(val);
        i += rep;
    }
    Ok((
        Huffman::new(&lengths[..nlen]),
        Huffman::new(&lengths[nlen..]),
    ))
}

/// Decode raw DEFLATE data, refusing to grow past `max` bytes.
pub fn inflate(data: &[u8], max: usize) -> Result<Vec<u8>> {
    let mut b = Bits {
        data,
        pos: 0,
        bit: 0,
    };
    let mut out = vec![];
    loop {
        let last = b.get(1)? == 1;
        match b.get(2)? {
            0 => {
                b.align();
                let Some(h) = data.get(b.pos..b.pos + 4) else {
                    bail!("compressed data ends early");
                };
                let len = u16::from_le_bytes([h[0], h[1]]) as usize;
                let nlen = u16::from_le_bytes([h[2], h[3]]) as usize;
                if len != !nlen & 0xffff {
                    bail!("stored block length check failed");
                }
                let start = b.pos + 4;
                let Some(chunk) = data.get(start..start + len) else {
                    bail!("compressed data ends early");
                };
                out.extend_from_slice(chunk);
                b.pos = start + len;
                if out.len() > max {
                    bail!("decompressed data is larger than {max} bytes");
                }
            }
            1 => {
                let (lit, dist) = fixed();
                codes(&mut b, &mut out, &lit, &dist, max)?;
            }
            2 => {
                let (lit, dist) = dynamic(&mut b)?;
                codes(&mut b, &mut out, &lit, &dist, max)?;
            }
            _ => bail!("bad block type in compressed data"),
        }
        if last {
            return Ok(out);
        }
    }
}

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &byte in data {
        c ^= byte as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// Decode a gzip file (one member), checking its CRC and length.
pub fn gunzip(data: &[u8], max: usize) -> Result<Vec<u8>> {
    if data.len() < 18 || data[0] != 0x1f || data[1] != 0x8b || data[2] != 8 {
        bail!("not a gzip file");
    }
    let flags = data[3];
    let mut pos = 10;
    if flags & 4 != 0 {
        let Some(x) = data.get(pos..pos + 2) else {
            bail!("gzip header ends early");
        };
        pos += 2 + u16::from_le_bytes([x[0], x[1]]) as usize;
    }
    for flag in [8u8, 16] {
        if flags & flag != 0 {
            let Some(end) = data.get(pos..).and_then(|r| r.iter().position(|&c| c == 0)) else {
                bail!("gzip header ends early");
            };
            pos += end + 1;
        }
    }
    if flags & 2 != 0 {
        pos += 2;
    }
    let Some(body) = data.get(pos..data.len() - 8) else {
        bail!("gzip header ends early");
    };
    let out = inflate(body, max)?;
    let t = &data[data.len() - 8..];
    let crc = u32::from_le_bytes([t[0], t[1], t[2], t[3]]);
    let size = u32::from_le_bytes([t[4], t[5], t[6], t[7]]);
    if crc32(&out) != crc || out.len() as u32 != size {
        bail!("gzip check failed: the file is damaged");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // made with Python's gzip module: gzip.compress(b"hello hello hello hello\n" * 3)
    const HELLO_GZ: &[u8] = &[
        0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0xff, 0xcb, 0x48, 0xcd, 0xc9, 0xc9,
        0x57, 0xc8, 0x40, 0x27, 0xb9, 0x32, 0x48, 0x14, 0x07, 0x00, 0x4a, 0x0e, 0xce, 0x11, 0x48,
        0x00, 0x00, 0x00,
    ];

    #[test]
    fn decodes_a_fixed_huffman_gzip() {
        let out = gunzip(HELLO_GZ, 1000).unwrap();
        assert_eq!(out, b"hello hello hello hello\n".repeat(3));
    }

    #[test]
    fn decodes_a_dynamic_huffman_stream() {
        let data = std::fs::read("tests/data/dynamic.gz").unwrap();
        let want = std::fs::read("tests/data/dynamic.txt").unwrap();
        assert_eq!(gunzip(&data, 1 << 20).unwrap(), want);
    }

    #[test]
    fn decodes_stored_blocks_and_refuses_damage() {
        // one final stored block holding "abc"
        let raw = [0x01, 0x03, 0x00, 0xfc, 0xff, b'a', b'b', b'c'];
        assert_eq!(inflate(&raw, 10).unwrap(), b"abc");
        assert!(inflate(&raw, 2).is_err(), "over the size cap");
        let mut bad = HELLO_GZ.to_vec();
        let n = bad.len();
        bad[n - 8] ^= 1;
        assert!(gunzip(&bad, 1000).is_err(), "CRC mismatch");
        assert!(gunzip(b"plain text, not gzip", 1000).is_err());
    }
}
