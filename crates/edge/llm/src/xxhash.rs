//! `XXH64` (the 64-bit xxHash by Yann Collet), hand-rolled for the Claude subscription `cch`
//! request checksum: small enough that a crate is not worth it.

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

const fn round(acc: u64, lane: u64) -> u64 {
    acc.wrapping_add(lane.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

const fn merge_round(acc: u64, lane: u64) -> u64 {
    (acc ^ round(0, lane)).wrapping_mul(P1).wrapping_add(P4)
}

/// `XXH64(input, seed)`.
pub(crate) fn xxh64(input: &[u8], seed: u64) -> u64 {
    let (stripes, rem) = input.as_chunks::<32>();
    let mut h = if stripes.is_empty() {
        seed.wrapping_add(P5)
    } else {
        let mut lanes = [
            seed.wrapping_add(P1).wrapping_add(P2),
            seed.wrapping_add(P2),
            seed,
            seed.wrapping_sub(P1),
        ];
        for stripe in stripes {
            for (lane, word) in lanes.iter_mut().zip(stripe.as_chunks::<8>().0) {
                *lane = round(*lane, u64::from_le_bytes(*word));
            }
        }
        let [v1, v2, v3, v4] = lanes;
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        for lane in lanes {
            h = merge_round(h, lane);
        }
        h
    };
    h = h.wrapping_add(u64::try_from(input.len()).unwrap_or(u64::MAX));

    let (words, mut tail) = rem.as_chunks::<8>();
    for word in words {
        h ^= round(0, u64::from_le_bytes(*word));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
    }
    if let Some((four, rest)) = tail.split_first_chunk::<4>() {
        h ^= u64::from(u32::from_le_bytes(*four)).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        tail = rest;
    }
    for &byte in tail {
        h ^= u64::from(byte).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
    }

    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;

    /// Reference values from `Bun.hash.xxHash64` (an independent `XXH64` implementation);
    /// the inputs cover the short path, the 4-byte and 8-byte tails and full 32-byte stripes.
    #[test]
    fn matches_reference_vectors() {
        let long = "0123456789abcdef".repeat(6) + "01234";
        let cases: [(&str, u64, u64); 5] = [
            ("", 0xef46_db37_51d8_e999, 0xb8b3_0e7d_e65b_46c5),
            ("a", 0xd24e_c4f1_a98c_6e5b, 0xde28_ae28_b0e0_7bb2),
            ("abc", 0x44bc_2cf5_ad77_0999, 0xdfc4_f4d6_9136_99b6),
            (
                "Nobody inspects the spammish repetition",
                0xfbce_a83c_8a37_8bf1,
                0xfc8d_069f_34aa_c4f8,
            ),
            (&long, 0x2221_4d41_d12d_3edd, 0x5f1c_9b2c_9276_829d),
        ];
        for (input, unseeded, seeded) in cases {
            assert_eq!(xxh64(input.as_bytes(), 0), unseeded, "{input:?} seed 0");
            assert_eq!(
                xxh64(input.as_bytes(), CCH_SEED),
                seeded,
                "{input:?} cch seed"
            );
        }
    }
}
