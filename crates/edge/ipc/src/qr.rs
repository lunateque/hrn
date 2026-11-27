//! Pure-Rust ISO/IEC 18004 QR code encoder (Byte mode, Error Correction Level L, Versions 1–14)
//! with compact UTF-8 half-block terminal and TUI renderers.

use hrn_kernel::{HrnError, Result};

const MAX_VERSION: usize = 14;
const GF_POLY: u8 = 0x1d;
const FORMAT_BCH_POLY: u32 = 0x537;
const FORMAT_MASK: u32 = 0x5412;
const FORMAT_EC_LEVEL_L: u32 = 0b01;
const VERSION_BCH_POLY: u32 = 0x1f25;

/// ISO/IEC 18004 Table 9 parameters for Error Correction Level L, Versions 1 through 14.
#[derive(Clone, Copy)]
struct VersionSpec {
    version: usize,
    total_cw: usize,
    ec_per_block: usize,
    num_blocks: usize,
}

impl VersionSpec {
    const fn new(version: usize, total_cw: usize, ec_per_block: usize, num_blocks: usize) -> Self {
        Self {
            version,
            total_cw,
            ec_per_block,
            num_blocks,
        }
    }
    const fn data_codewords(self) -> usize {
        self.total_cw - self.ec_per_block * self.num_blocks
    }
    const fn count_bits(self) -> usize {
        if self.version <= 9 { 8 } else { 16 }
    }
    const fn byte_capacity(self) -> usize {
        (self.data_codewords() * 8 - 4 - self.count_bits()) / 8
    }
    const fn size(self) -> usize {
        17 + 4 * self.version
    }
}

const VERSION_SPECS: [VersionSpec; MAX_VERSION] = [
    VersionSpec::new(1, 26, 7, 1),
    VersionSpec::new(2, 44, 10, 1),
    VersionSpec::new(3, 70, 15, 1),
    VersionSpec::new(4, 100, 20, 1),
    VersionSpec::new(5, 134, 26, 1),
    VersionSpec::new(6, 172, 18, 2),
    VersionSpec::new(7, 196, 20, 2),
    VersionSpec::new(8, 242, 24, 2),
    VersionSpec::new(9, 292, 30, 2),
    VersionSpec::new(10, 346, 18, 4),
    VersionSpec::new(11, 404, 20, 4),
    VersionSpec::new(12, 466, 24, 4),
    VersionSpec::new(13, 532, 26, 4),
    VersionSpec::new(14, 581, 30, 4),
];

/// Square QR code module matrix (`true` = dark module, `false` = light module).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrMatrix {
    size: usize,
    modules: Vec<bool>,
}

impl QrMatrix {
    /// Encode a UTF-8 string into the smallest fitting Version 1–14 Level L QR code.
    pub fn encode(text: &str) -> Result<Self> {
        Self::encode_bytes(text.as_bytes())
    }

    /// Encode arbitrary raw bytes into the smallest fitting Version 1–14 Level L QR code.
    pub fn encode_bytes(data: &[u8]) -> Result<Self> {
        let spec = select_version(data.len())?;
        let data_cw = encode_byte_mode(data, spec)?;
        let final_cw = build_interleaved_codewords(&data_cw, spec);
        let size = spec.size();
        let mut grid = Grid::new(size);
        draw_function_patterns(&mut grid, spec.version);
        draw_codewords(&mut grid, &final_cw);

        let mut best_penalty = u32::MAX;
        let mut best_modules = Vec::new();
        for mask in 0u32..8 {
            let mut candidate = grid.clone();
            apply_mask(&mut candidate, mask);
            draw_format_bits(&mut candidate, mask);
            let penalty = penalty_score(&candidate);
            if penalty < best_penalty {
                best_penalty = penalty;
                best_modules = candidate.modules;
            }
        }
        Ok(Self {
            size,
            modules: best_modules,
        })
    }

    /// Width and height of the square matrix in modules (`17 + 4 * version`).
    pub fn size(&self) -> usize {
        self.size
    }

    /// Return `true` when the module at column `x`, row `y` is dark (`false` if out of bounds).
    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.size
            && y < self.size
            && self
                .modules
                .get(y * self.size + x)
                .copied()
                .unwrap_or(false)
    }

    /// Render the matrix as compact UTF-8 half-block lines (`█`, `▀`, `▄`, ` `) with `quiet_zone`
    /// light modules on every edge.
    ///
    /// When `invert` is `false`, dark modules use the foreground glyph (for black-on-white TUI or
    /// ANSI styling). When `invert` is `true`, light modules use the foreground glyph (for plain
    /// dark terminals without ANSI background colors).
    pub fn render_half_blocks_lines(&self, quiet_zone: usize, invert: bool) -> Vec<String> {
        let total = self.size.saturating_add(quiet_zone.saturating_mul(2));
        let mut lines = Vec::with_capacity(total.div_ceil(2));
        for row in (0..total).step_by(2) {
            let mut line = String::with_capacity(total * 3);
            for col in 0..total {
                let top_fg = self.module_with_quiet(col, row, quiet_zone) ^ invert;
                let bot_fg =
                    (row + 1 < total && self.module_with_quiet(col, row + 1, quiet_zone)) ^ invert;
                line.push(match (top_fg, bot_fg) {
                    (true, true) => '█',
                    (true, false) => '▀',
                    (false, true) => '▄',
                    (false, false) => ' ',
                });
            }
            lines.push(line);
        }
        lines
    }

    /// Render the QR code with explicit ANSI black-on-white contrast (`\x1b[47;30m`) and a
    /// 2-module quiet zone so phone cameras scan reliably on any terminal theme.
    pub fn render_ansi_dark_on_light(&self) -> String {
        let mut out = String::new();
        for line in self.render_half_blocks_lines(2, false) {
            out.push_str("\x1b[47;30m");
            out.push_str(&line);
            out.push_str("\x1b[0m\n");
        }
        out
    }

    fn module_with_quiet(&self, col: usize, row: usize, quiet: usize) -> bool {
        col >= quiet && row >= quiet && self.is_dark(col - quiet, row - quiet)
    }
}

/// Render `matrix` as UTF-8 half-block lines where dark modules map to foreground glyphs.
pub fn render_half_blocks(matrix: &QrMatrix, quiet_zone: usize) -> Vec<String> {
    matrix.render_half_blocks_lines(quiet_zone, false)
}
/// Render `matrix` as UTF-8 half-block lines where light modules map to foreground glyphs
/// (white quiet zone and light modules on a dark terminal background).
pub fn render_half_blocks_inverted(matrix: &QrMatrix, quiet_zone: usize) -> Vec<String> {
    matrix.render_half_blocks_lines(quiet_zone, true)
}
#[derive(Clone)]
struct Grid {
    size: usize,
    modules: Vec<bool>,
    is_func: Vec<bool>,
}
impl Grid {
    fn new(size: usize) -> Self {
        Self {
            size,
            modules: vec![false; size * size],
            is_func: vec![false; size * size],
        }
    }
    fn get(&self, x: usize, y: usize) -> bool {
        self.modules
            .get(y * self.size + x)
            .copied()
            .unwrap_or(false)
    }
    fn is_function(&self, x: usize, y: usize) -> bool {
        self.is_func.get(y * self.size + x).copied().unwrap_or(true)
    }
    fn set_func(&mut self, x: usize, y: usize, dark: bool) {
        let idx = y * self.size + x;
        if let Some(slot) = self.modules.get_mut(idx) {
            *slot = dark;
        }
        if let Some(flag) = self.is_func.get_mut(idx) {
            *flag = true;
        }
    }
    fn set_data(&mut self, x: usize, y: usize, dark: bool) {
        if let Some(slot) = self.modules.get_mut(y * self.size + x) {
            *slot = dark;
        }
    }
    fn flip_data(&mut self, x: usize, y: usize) {
        if let Some(slot) = self.modules.get_mut(y * self.size + x) {
            *slot = !*slot;
        }
    }
}

fn select_version(payload_len: usize) -> Result<VersionSpec> {
    if let Some(spec) = VERSION_SPECS
        .into_iter()
        .find(|s| payload_len <= s.byte_capacity())
    {
        return Ok(spec);
    }
    let max_cap = VERSION_SPECS.last().map_or(0, |s| s.byte_capacity());
    Err(HrnError::Config(format!(
        "QR payload of {payload_len} bytes exceeds Version {MAX_VERSION}-L capacity ({max_cap} bytes)"
    )))
}

fn push_bits(bytes: &mut Vec<u8>, bit_len: &mut usize, val: u32, count: usize) {
    for shift in (0..count).rev() {
        if (*bit_len).is_multiple_of(8) {
            bytes.push(0);
        }
        if ((val >> shift) & 1) != 0
            && let Some(last) = bytes.last_mut()
        {
            *last |= 1u8 << (7 - (*bit_len % 8));
        }
        *bit_len += 1;
    }
}

fn encode_byte_mode(data: &[u8], spec: VersionSpec) -> Result<Vec<u8>> {
    let data_cw = spec.data_codewords();
    let payload_len = u32::try_from(data.len())
        .map_err(|_| HrnError::Config("QR payload length overflow".to_owned()))?;
    let mut bytes = Vec::with_capacity(data_cw);
    let mut bit_len = 0usize;
    push_bits(&mut bytes, &mut bit_len, 0b0100, 4);
    push_bits(&mut bytes, &mut bit_len, payload_len, spec.count_bits());
    for &b in data {
        push_bits(&mut bytes, &mut bit_len, u32::from(b), 8);
    }
    let term = (data_cw * 8).saturating_sub(bit_len).min(4);
    push_bits(&mut bytes, &mut bit_len, 0, term);
    let pad = (8 - (bit_len % 8)) % 8;
    if pad > 0 {
        push_bits(&mut bytes, &mut bit_len, 0, pad);
    }
    let mut pad_toggle = false;
    while bytes.len() < data_cw {
        bytes.push(if pad_toggle { 0x11 } else { 0xec });
        pad_toggle = !pad_toggle;
    }
    Ok(bytes)
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut prod: u8 = 0;
    for _ in 0..8 {
        if (b & 1) != 0 {
            prod ^= a;
        }
        let hi = (a & 0x80) != 0;
        a <<= 1;
        if hi {
            a ^= GF_POLY;
        }
        b >>= 1;
    }
    prod
}

fn rs_generator(degree: usize) -> Vec<u8> {
    let mut poly = vec![0u8; degree];
    if let Some(last) = poly.last_mut() {
        *last = 1;
    }
    let mut root: u8 = 1;
    for _ in 0..degree {
        for j in 0..degree {
            let cur = poly.get(j).copied().unwrap_or(0);
            let next = poly.get(j + 1).copied().unwrap_or(0);
            if let Some(slot) = poly.get_mut(j) {
                *slot = gf_mul(cur, root) ^ next;
            }
        }
        root = gf_mul(root, 2);
    }
    poly
}

fn rs_remainder(data: &[u8], poly: &[u8]) -> Vec<u8> {
    let deg = poly.len();
    let mut rem = vec![0u8; deg];
    for &byte in data {
        let factor = byte ^ rem.first().copied().unwrap_or(0);
        for j in 0..deg {
            let shifted = rem.get(j + 1).copied().unwrap_or(0);
            let coeff = poly.get(j).copied().unwrap_or(0);
            if let Some(slot) = rem.get_mut(j) {
                *slot = shifted ^ gf_mul(coeff, factor);
            }
        }
    }
    rem
}

fn build_interleaved_codewords(data_cw: &[u8], spec: VersionSpec) -> Vec<u8> {
    let g2_blocks = spec.total_cw % spec.num_blocks;
    let g1_blocks = spec.num_blocks - g2_blocks;
    let g1_data = (spec.total_cw / spec.num_blocks) - spec.ec_per_block;
    let poly = rs_generator(spec.ec_per_block);
    let mut data_blocks: Vec<&[u8]> = Vec::with_capacity(spec.num_blocks);
    let mut ec_blocks: Vec<Vec<u8>> = Vec::with_capacity(spec.num_blocks);
    let mut offset = 0usize;
    for blk in 0..spec.num_blocks {
        let len = if blk < g1_blocks {
            g1_data
        } else {
            g1_data + 1
        };
        let slice = data_cw.get(offset..offset + len).unwrap_or_default();
        offset += len;
        ec_blocks.push(rs_remainder(slice, &poly));
        data_blocks.push(slice);
    }
    let mut out = Vec::with_capacity(spec.total_cw);
    let max_data = g1_data + usize::from(g2_blocks > 0);
    for i in 0..max_data {
        for blk in &data_blocks {
            if let Some(&b) = blk.get(i) {
                out.push(b);
            }
        }
    }
    for i in 0..spec.ec_per_block {
        for ec in &ec_blocks {
            if let Some(&b) = ec.get(i) {
                out.push(b);
            }
        }
    }
    out
}

fn alignment_centers(version: usize) -> Vec<usize> {
    if version <= 1 {
        return Vec::new();
    }
    let count = version / 7 + 2;
    let step = ((version * 4 + count * 2 + 1) / (count * 2 - 2)) * 2;
    let last = version * 4 + 10;
    let mut centers = vec![6usize];
    let mut tail: Vec<usize> = (0..(count - 1)).map(|i| last - i * step).collect();
    tail.reverse();
    centers.extend(tail);
    centers
}

fn draw_function_patterns(grid: &mut Grid, version: usize) {
    let size = grid.size;
    for i in 0..size {
        grid.set_func(6, i, i.is_multiple_of(2));
        grid.set_func(i, 6, i.is_multiple_of(2));
    }
    for (cx, cy) in [(3, 3), (size - 4, 3), (3, size - 4)] {
        draw_finder(grid, cx, cy);
    }
    let centers = alignment_centers(version);
    let last = centers.len().saturating_sub(1);
    for (iy, &cy) in centers.iter().enumerate() {
        for (ix, &cx) in centers.iter().enumerate() {
            let corner = (ix == 0 && (iy == 0 || iy == last)) || (ix == last && iy == 0);
            if !corner {
                draw_alignment(grid, cx, cy);
            }
        }
    }
    draw_format_bits(grid, 0);
    if version >= 7 {
        draw_version_bits(grid, version);
    }
}

fn draw_finder(grid: &mut Grid, cx: usize, cy: usize) {
    let size = grid.size;
    for dy in -4isize..=4 {
        for dx in -4isize..=4 {
            if let (Some(x), Some(y)) = (cx.checked_add_signed(dx), cy.checked_add_signed(dy))
                && x < size
                && y < size
            {
                let dist = dx.unsigned_abs().max(dy.unsigned_abs());
                grid.set_func(x, y, dist != 2 && dist != 4);
            }
        }
    }
}

fn draw_alignment(grid: &mut Grid, cx: usize, cy: usize) {
    for dy in -2isize..=2 {
        for dx in -2isize..=2 {
            if let (Some(x), Some(y)) = (cx.checked_add_signed(dx), cy.checked_add_signed(dy)) {
                grid.set_func(x, y, dx.unsigned_abs().max(dy.unsigned_abs()) != 1);
            }
        }
    }
}

fn bch_remainder(mut val: u32, poly: u32, poly_bits: u32) -> u32 {
    let top_bit = 1u32 << (poly_bits - 1);
    for shift in (0..=(31 - poly_bits)).rev() {
        if (val & (top_bit << shift)) != 0 {
            val ^= poly << shift;
        }
    }
    val
}

fn draw_format_bits(grid: &mut Grid, mask: u32) {
    let data = (FORMAT_EC_LEVEL_L << 3) | (mask & 0b111);
    let bits = ((data << 10) | bch_remainder(data << 10, FORMAT_BCH_POLY, 11)) ^ FORMAT_MASK;
    let size = grid.size;
    for i in 0usize..=5 {
        grid.set_func(8, i, ((bits >> i) & 1) != 0);
    }
    grid.set_func(8, 7, ((bits >> 6) & 1) != 0);
    grid.set_func(8, 8, ((bits >> 7) & 1) != 0);
    grid.set_func(7, 8, ((bits >> 8) & 1) != 0);
    for i in 9usize..15 {
        grid.set_func(14 - i, 8, ((bits >> i) & 1) != 0);
    }
    for i in 0usize..8 {
        grid.set_func(size - 1 - i, 8, ((bits >> i) & 1) != 0);
    }
    for i in 8usize..15 {
        grid.set_func(8, size - 15 + i, ((bits >> i) & 1) != 0);
    }
    grid.set_func(8, size - 8, true);
}

fn draw_version_bits(grid: &mut Grid, version: usize) {
    let ver = u32::try_from(version).unwrap_or(0);
    let bits = (ver << 12) | bch_remainder(ver << 12, VERSION_BCH_POLY, 13);
    let size = grid.size;
    for i in 0usize..18 {
        let dark = ((bits >> i) & 1) != 0;
        let (a, b) = (size - 11 + (i % 3), i / 3);
        grid.set_func(b, a, dark);
        grid.set_func(a, b, dark);
    }
}

fn draw_codewords(grid: &mut Grid, codewords: &[u8]) {
    let size = grid.size;
    let total_bits = codewords.len() * 8;
    let (mut bit_idx, mut right, mut upward) = (0usize, size - 1, true);
    loop {
        if right == 6 {
            right = 5;
        }
        for step in 0..size {
            let y = if upward { size - 1 - step } else { step };
            for dx in 0usize..2 {
                let x = right - dx;
                if !grid.is_function(x, y) {
                    let dark = bit_idx < total_bits
                        && ((codewords.get(bit_idx / 8).copied().unwrap_or(0)
                            >> (7 - (bit_idx % 8)))
                            & 1)
                            != 0;
                    grid.set_data(x, y, dark);
                    bit_idx += 1;
                }
            }
        }
        upward = !upward;
        if right < 2 {
            break;
        }
        right -= 2;
    }
}

fn apply_mask(grid: &mut Grid, mask: u32) {
    for y in 0..grid.size {
        for x in 0..grid.size {
            if !grid.is_function(x, y) && mask_bit(mask, x, y) {
                grid.flip_data(x, y);
            }
        }
    }
}

fn mask_bit(mask: u32, x: usize, y: usize) -> bool {
    match mask {
        0 => (x + y).is_multiple_of(2),
        1 => y.is_multiple_of(2),
        2 => x.is_multiple_of(3),
        3 => (x + y).is_multiple_of(3),
        4 => (y / 2 + x / 3).is_multiple_of(2),
        5 => (x * y) % 2 + (x * y) % 3 == 0,
        6 => ((x * y) % 2 + (x * y) % 3).is_multiple_of(2),
        _ => ((x + y) % 2 + (x * y) % 3).is_multiple_of(2),
    }
}

fn penalty_score(grid: &Grid) -> u32 {
    let size = grid.size;
    let mut score = 0u32;
    for a in 0..size {
        score += line_penalty(size, |b| grid.get(b, a)) + line_penalty(size, |b| grid.get(a, b));
    }
    for y in 0..(size - 1) {
        for x in 0..(size - 1) {
            let c = grid.get(x, y);
            if c == grid.get(x + 1, y) && c == grid.get(x, y + 1) && c == grid.get(x + 1, y + 1) {
                score += 3;
            }
        }
    }
    let dark = grid.modules.iter().filter(|&&m| m).count();
    let total = size * size;
    score + u32::try_from((dark * 20).abs_diff(total * 10) / total).unwrap_or(0) * 10
}

fn line_penalty(len: usize, at: impl Fn(usize) -> bool) -> u32 {
    let (mut score, mut run_color, mut run_len, mut window) = (0u32, false, 0u32, 0u32);
    for i in 0..len {
        let dark = at(i);
        if i == 0 || dark != run_color {
            run_color = dark;
            run_len = 1;
        } else {
            run_len += 1;
            if run_len == 5 {
                score += 3;
            } else if run_len > 5 {
                score += 1;
            }
        }
        window = ((window << 1) | u32::from(dark)) & 0x7ff;
        if i >= 10 && (window == 0b101_1101_0000 || window == 0b000_0101_1101) {
            score += 40;
        }
    }
    score
}

#[cfg(test)]
#[path = "qr_tests.rs"]
mod tests;
