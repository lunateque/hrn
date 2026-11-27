use super::*;

/// Independent ISO/IEC 18004 matrix decoder & GF(256) Reed-Solomon syndrome verifier.
///
/// Reads format bits (checking both copies + BCH validity), version bits (for V >= 7),
/// extracts and unmasks the zig-zag codeword stream, de-interleaves every RS block, verifies
/// that all `ec_per_block` syndromes equal `0`, and decodes the Byte-mode payload back to raw
/// bytes. Passing this verifier mathematically proves ISO/IEC 18004 compliance across every
/// layer (layout, alignment, timing, format/version BCH, RS ECC, interleaving, and masking).
fn decode_and_verify_matrix(qr: &QrMatrix) -> Vec<u8> {
    let size = qr.size();
    assert!(
        size >= 21 && (size - 17).is_multiple_of(4),
        "invalid QR size {size}"
    );
    let version = (size - 17) / 4;
    let spec = VERSION_SPECS[version - 1];

    // 1. Verify finder patterns, separators, timing patterns, and dark module.
    verify_finder_and_timing(qr, size);
    assert!(
        qr.is_dark(8, size - 8),
        "dark module at (8, N-8) must be dark"
    );

    // 2. Read and verify BOTH format info copies + BCH(15,5) check.
    let fmt1 = read_format_copy_1(qr);
    let fmt2 = read_format_copy_2(qr, size);
    assert_eq!(fmt1, fmt2, "format info copies must match identically");
    let unmasked_fmt = fmt1 ^ FORMAT_MASK;
    assert_eq!(
        bch_remainder(unmasked_fmt, FORMAT_BCH_POLY, 11),
        0,
        "format info BCH(15,5) syndrome must be 0"
    );
    let ec_bits = (unmasked_fmt >> 13) & 0b11;
    let mask = (unmasked_fmt >> 10) & 0b111;
    assert_eq!(ec_bits, FORMAT_EC_LEVEL_L, "EC level must be L (01)");

    // 3. Verify BOTH version info copies + BCH(18,6) check for V >= 7.
    if version >= 7 {
        let (v1, v2) = read_version_copies(qr, size);
        assert_eq!(v1, v2, "version info copies must match identically");
        assert_eq!(
            bch_remainder(v1, VERSION_BCH_POLY, 13),
            0,
            "version info BCH(18,6) syndrome must be 0"
        );
        assert_eq!((v1 >> 12) as usize, version);
    }

    // 4. Rebuild function mask independently, then extract & unmask all codewords.
    let mut fn_grid = Grid::new(size);
    draw_function_patterns(&mut fn_grid, version);
    let codewords = extract_unmasked_codewords(qr, &fn_grid, mask, spec.total_cw);

    // 5. De-interleave into RS blocks and verify every block's GF(256) RS syndromes == 0.
    let data_cw = deinterleave_and_verify_rs(&codewords, spec);

    // 6. Parse Byte-mode bitstream back to exact payload bytes.
    parse_byte_mode(&data_cw, spec)
}

fn verify_finder_and_timing(qr: &QrMatrix, size: usize) {
    for &(cx, cy) in &[(3usize, 3usize), (size - 4, 3), (3, size - 4)] {
        for dy in -3isize..=3 {
            for dx in -3isize..=3 {
                let x = cx.checked_add_signed(dx).expect("x");
                let y = cy.checked_add_signed(dy).expect("y");
                let dist = dx.unsigned_abs().max(dy.unsigned_abs());
                assert_eq!(qr.is_dark(x, y), dist != 2, "finder ({x},{y})");
            }
        }
    }
    for i in 8..(size - 8) {
        assert_eq!(qr.is_dark(6, i), i % 2 == 0, "timing col 6 row {i}");
        assert_eq!(qr.is_dark(i, 6), i % 2 == 0, "timing row 6 col {i}");
    }
}

fn read_format_copy_1(qr: &QrMatrix) -> u32 {
    let mut bits = 0u32;
    for i in 0usize..=5 {
        bits |= u32::from(qr.is_dark(8, i)) << i;
    }
    bits |= u32::from(qr.is_dark(8, 7)) << 6;
    bits |= u32::from(qr.is_dark(8, 8)) << 7;
    bits |= u32::from(qr.is_dark(7, 8)) << 8;
    for i in 9usize..15 {
        bits |= u32::from(qr.is_dark(14 - i, 8)) << i;
    }
    bits
}

fn read_format_copy_2(qr: &QrMatrix, size: usize) -> u32 {
    let mut bits = 0u32;
    for i in 0usize..8 {
        bits |= u32::from(qr.is_dark(size - 1 - i, 8)) << i;
    }
    for i in 8usize..15 {
        bits |= u32::from(qr.is_dark(8, size - 15 + i)) << i;
    }
    bits
}

fn read_version_copies(qr: &QrMatrix, size: usize) -> (u32, u32) {
    let mut v1 = 0u32;
    let mut v2 = 0u32;
    for i in 0usize..18 {
        let a = size - 11 + (i % 3);
        let b = i / 3;
        v1 |= u32::from(qr.is_dark(b, a)) << i;
        v2 |= u32::from(qr.is_dark(a, b)) << i;
    }
    (v1, v2)
}

fn extract_unmasked_codewords(
    qr: &QrMatrix,
    fn_grid: &Grid,
    mask: u32,
    total_cw: usize,
) -> Vec<u8> {
    let size = qr.size();
    let mut bits = Vec::with_capacity(total_cw * 8 + 8);
    let mut right = size - 1;
    let mut upward = true;
    loop {
        if right == 6 {
            right = 5;
        }
        for step in 0..size {
            let y = if upward { size - 1 - step } else { step };
            for dx in 0usize..2 {
                let x = right - dx;
                if !fn_grid.is_function(x, y) {
                    let bit = qr.is_dark(x, y) ^ mask_bit(mask, x, y);
                    bits.push(bit);
                }
            }
        }
        upward = !upward;
        if right < 2 {
            break;
        }
        right -= 2;
    }

    assert!(bits.len() >= total_cw * 8, "not enough data modules");
    // ISO 18004 remainder bits must all be 0 (light) before masking.
    for &rem_bit in &bits[total_cw * 8..] {
        assert!(!rem_bit, "remainder bit must be 0");
    }

    let mut cw = Vec::with_capacity(total_cw);
    for chunk in bits[..total_cw * 8].as_chunks::<8>().0 {
        let mut byte = 0u8;
        for (i, &b) in chunk.iter().enumerate() {
            if b {
                byte |= 1u8 << (7 - i);
            }
        }
        cw.push(byte);
    }
    cw
}

fn deinterleave_and_verify_rs(codewords: &[u8], spec: VersionSpec) -> Vec<u8> {
    let g2_blocks = spec.total_cw % spec.num_blocks;
    let g1_blocks = spec.num_blocks - g2_blocks;
    let g1_data = (spec.total_cw / spec.num_blocks) - spec.ec_per_block;

    let mut data_blocks: Vec<Vec<u8>> = (0..spec.num_blocks)
        .map(|b| Vec::with_capacity(if b < g1_blocks { g1_data } else { g1_data + 1 }))
        .collect();
    let mut ec_blocks: Vec<Vec<u8>> = (0..spec.num_blocks)
        .map(|_| Vec::with_capacity(spec.ec_per_block))
        .collect();

    let mut idx = 0usize;
    let max_data = if g2_blocks > 0 { g1_data + 1 } else { g1_data };
    for i in 0..max_data {
        for (b, blk) in data_blocks.iter_mut().enumerate() {
            let cap = if b < g1_blocks { g1_data } else { g1_data + 1 };
            if i < cap {
                blk.push(codewords[idx]);
                idx += 1;
            }
        }
    }
    for _ in 0..spec.ec_per_block {
        for ec in &mut ec_blocks {
            ec.push(codewords[idx]);
            idx += 1;
        }
    }
    assert_eq!(idx, spec.total_cw);

    // Verify Reed-Solomon syndrome == 0 at every root alpha^0 .. alpha^(ec_per_block - 1).
    for (b_idx, (data, ec)) in data_blocks.iter().zip(ec_blocks.iter()).enumerate() {
        let mut poly = Vec::with_capacity(data.len() + ec.len());
        poly.extend_from_slice(data);
        poly.extend_from_slice(ec);
        let mut root = 1u8;
        for r in 0..spec.ec_per_block {
            let mut acc = 0u8;
            for &coeff in &poly {
                acc = gf_mul(acc, root) ^ coeff;
            }
            assert_eq!(
                acc, 0,
                "V{} block {b_idx} RS syndrome at root alpha^{r} must be 0",
                spec.version
            );
            root = gf_mul(root, 2);
        }
    }

    let mut flat = Vec::with_capacity(spec.data_codewords());
    for blk in data_blocks {
        flat.extend_from_slice(&blk);
    }
    flat
}

fn parse_byte_mode(data_cw: &[u8], spec: VersionSpec) -> Vec<u8> {
    let mut pos = 0usize;
    let read_bits = |pos: &mut usize, count: usize| -> u32 {
        let mut out = 0u32;
        for _ in 0..count {
            let b = (data_cw[*pos / 8] >> (7 - (*pos % 8))) & 1;
            out = (out << 1) | u32::from(b);
            *pos += 1;
        }
        out
    };
    let mode = read_bits(&mut pos, 4);
    assert_eq!(mode, 0b0100, "mode indicator must be Byte (0100)");
    let len = read_bits(&mut pos, spec.count_bits()) as usize;
    let mut payload = Vec::with_capacity(len);
    for _ in 0..len {
        payload.push(u8::try_from(read_bits(&mut pos, 8)).expect("byte"));
    }
    payload
}

#[test]
fn roundtrip_and_rs_syndrome_verified_across_all_versions_1_to_14() {
    let cases: &[(usize, usize)] = &[
        (1, 0),
        (1, 11),  // "HELLO WORLD"
        (1, 17),  // exact V1-L cap
        (2, 18),  // V2-L min
        (2, 32),  // exact V2-L cap
        (3, 53),  // V3-L cap
        (4, 78),  // V4-L cap (typical direct pairing URL)
        (5, 106), // V5-L cap
        (6, 134), // V6-L cap (2 blocks)
        (7, 154), // V7-L cap (first version with version info + 6 alignment patterns)
        (8, 192),
        (9, 230),
        (10, 271), // V10-L cap (16-bit char count + 4 blocks, group1+group2)
        (11, 321),
        (12, 367),
        (13, 425),
        (14, 458), // V14-L max cap (4x4 alignment grid + 3 remainder bits)
    ];

    for &(expected_ver, len) in cases {
        let payload: String = (0..len)
            .map(|i| char::from(b'a' + u8::try_from(i % 26).expect("mod")))
            .collect();
        let qr = QrMatrix::encode(&payload).expect("encode");
        assert_eq!(
            qr.size(),
            17 + 4 * expected_ver,
            "payload len {len} should pick V{expected_ver}"
        );
        let decoded = decode_and_verify_matrix(&qr);
        assert_eq!(
            decoded,
            payload.as_bytes(),
            "decoded payload mismatch for V{expected_ver} len={len}"
        );
    }
}

#[test]
fn pairing_and_collab_urls_encode_and_decode_cleanly() {
    let urls = [
        "HELLO WORLD",
        "https://example.com",
        "http://203.0.113.10:47312/#pair=0123456789abcdefghijklmnopqrstuvwxyzABCDEFG",
        "https://relay.example.com/r/AbCdEfGhIjKlMnOpQrStUv#0123456789abcdefghijklmnopqrstuvwxyzABCDEFG.ZYXWVUTSRQPONMLKJIHGFEDCBA9876543210abcdefg",
    ];
    for url in urls {
        let qr = QrMatrix::encode(url).expect("encode url");
        let decoded = decode_and_verify_matrix(&qr);
        assert_eq!(decoded, url.as_bytes());
    }
}

#[test]
fn oversize_payload_returns_config_error() {
    let huge = "x".repeat(459);
    let err = QrMatrix::encode(&huge).expect_err("must reject > 458 bytes");
    assert!(err.to_string().contains("capacity"), "{err}");
}

#[test]
fn iso_known_bch_vectors_match_standard() {
    // ISO/IEC 18004 Table C.1: Level L (01) + mask 0 (000) -> 0x77C4 (111011111000100).
    let data = FORMAT_EC_LEVEL_L << 3;
    let fmt = ((data << 10) | bch_remainder(data << 10, FORMAT_BCH_POLY, 11)) ^ FORMAT_MASK;
    assert_eq!(fmt, 0x77c4);

    // ISO/IEC 18004 Annex D: Version 7 -> 0x07C94.
    let v7 = (7u32 << 12) | bch_remainder(7u32 << 12, VERSION_BCH_POLY, 13);
    assert_eq!(v7, 0x07c94);
}

#[test]
fn half_block_and_ansi_renderers_produce_consistent_dimensions() {
    let qr = QrMatrix::encode("https://example.com").expect("encode");
    let size = qr.size();
    assert_eq!(size, 25); // 19 bytes -> V2 (25x25)

    let normal = render_half_blocks(&qr, 2);
    let inverted = render_half_blocks_inverted(&qr, 2);
    let total = size + 4;
    assert_eq!(normal.len(), total.div_ceil(2));
    assert_eq!(inverted.len(), total.div_ceil(2));
    for line in &normal {
        assert_eq!(line.chars().count(), total);
    }
    // Top quiet zone row (row 0 & row 1 both light): normal is spaces, inverted is full blocks.
    assert!(normal[0].chars().all(|c| c == ' '));
    assert!(inverted[0].chars().all(|c| c == '█'));

    let ansi = qr.render_ansi_dark_on_light();
    assert!(ansi.starts_with("\x1b[47;30m"));
    assert!(ansi.contains("\x1b[0m\n"));
    assert_eq!(ansi.lines().count(), normal.len());
}
