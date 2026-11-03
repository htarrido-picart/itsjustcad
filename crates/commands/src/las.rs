// SPDX-License-Identifier: AGPL-3.0-only
// Copyright © 2026 Hector Tarrido-Picart

//! Minimal LAS 1.2–1.4 parser for point-cloud import, with LAZ decompression.
//!
//! Reads the public header block to extract scale/offset and the number of
//! point records, then decodes X/Y/Z integer triples from point formats 0–10
//! (which all start with X i32, Y i32, Z i32).  Returns `(positions, decimation_stride)`.
//!
//! Compressed LAZ (point format with bit 7 set) is handled through the pure-Rust
//! `laz` crate (laz-rs, Apache-2.0): the "laszip encoded" VLR (record 22204) is
//! located, its payload parsed into a [`laz::LazVlr`], and records are
//! decompressed chunk-wise so decimation never materializes the whole cloud.
//! Chunks decode in parallel, a bounded batch at a time (#271).

use glam::DVec3;
use rayon::prelude::*;

/// Maximum points kept after decimation.
pub const MAX_POINTS: usize = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum LasError {
    #[error("file too short to contain a LAS header")]
    TooShort,
    #[error("not a LAS file: signature is {0:?}, expected \"LASF\"")]
    BadSignature([u8; 4]),
    #[error("LAZ file has no laszip VLR (record 22204) — cannot locate compression parameters")]
    LazVlrMissing,
    #[error("LAZ decompression failed: {0}")]
    Laz(String),
    #[error("unsupported point data format {0} (supported: 0–10)")]
    UnsupportedFormat(u8),
    #[error("LAS header version {major}.{minor} is not 1.2–1.4")]
    UnsupportedVersion { major: u8, minor: u8 },
    #[error("point data offset {offset} is past end of file ({len} bytes)")]
    BadOffset { offset: u32, len: usize },
    #[error("LAS header contains non-finite scale or offset values (file is corrupt)")]
    NonFiniteHeader,
}

/// Parsed output: world-space positions (after applying scale and offset) and
/// the stride used for decimation (1 = no decimation).
#[derive(Debug)]
pub struct LasPoints {
    pub positions: Vec<DVec3>,
    /// Stride applied when reading: every `stride`-th record was kept.
    pub stride: usize,
    pub total_records: u64,
}

/// Parse raw LAS bytes into a decimated set of positions.
pub fn parse(data: &[u8]) -> Result<LasPoints, LasError> {
    if data.len() < 227 {
        return Err(LasError::TooShort);
    }

    // Byte 0–3: file signature "LASF"
    let sig: [u8; 4] = data[0..4].try_into().unwrap();
    if &sig != b"LASF" {
        return Err(LasError::BadSignature(sig));
    }

    let major = data[24];
    let minor = data[25];
    if major != 1 || !(2..=4).contains(&minor) {
        return Err(LasError::UnsupportedVersion { major, minor });
    }

    // Header size: bytes 94–95 (u16 LE).  Minimum 227 for 1.2/1.3, 375 for 1.4.
    let header_size = u16::from_le_bytes([data[94], data[95]]) as usize;
    if data.len() < header_size {
        return Err(LasError::TooShort);
    }

    // Point data format ID: byte 104.
    let point_format = data[104];
    // LAZ sets bit 7 in the format byte (0x80 | format); the low bits keep the
    // uncompressed record layout.
    let is_laz = point_format & 0x80 != 0;
    let raw_format = point_format & 0x7f;
    if raw_format > 10 {
        return Err(LasError::UnsupportedFormat(raw_format));
    }

    // Point data record length: bytes 105–106.
    let record_length = u16::from_le_bytes([data[105], data[106]]) as usize;
    if record_length < 20 {
        // Minimum: X(4) Y(4) Z(4) intensity(2) flags(1) class(1) scan_angle(1)
        // user_data(1) point_source_id(2) = 20 bytes for format 0.
        return Err(LasError::TooShort);
    }

    // Offset to point data: bytes 96–99 (u32 LE).
    let point_offset = u32::from_le_bytes(data[96..100].try_into().unwrap()) as usize;
    if point_offset > data.len() {
        return Err(LasError::BadOffset { offset: point_offset as u32, len: data.len() });
    }

    // Number of point records.
    // 1.2/1.3: legacy count at bytes 107–110 (u32 LE).
    // 1.4:     u64 at bytes 247–254; fall back to legacy u32 for simplicity since
    //          the u64 field may be 0 in files that set only the legacy field.
    let legacy_count = u32::from_le_bytes(data[107..111].try_into().unwrap()) as u64;
    let total_records: u64 = if minor >= 4 && header_size >= 375 {
        let cnt64 = u64::from_le_bytes(data[247..255].try_into().unwrap());
        if cnt64 > 0 { cnt64 } else { legacy_count }
    } else {
        legacy_count
    };

    // Scale factors and offsets: bytes 131–178 (3×f64 scale + 3×f64 offset).
    let sx = f64::from_le_bytes(data[131..139].try_into().unwrap());
    let sy = f64::from_le_bytes(data[139..147].try_into().unwrap());
    let sz = f64::from_le_bytes(data[147..155].try_into().unwrap());
    let ox = f64::from_le_bytes(data[155..163].try_into().unwrap());
    let oy = f64::from_le_bytes(data[163..171].try_into().unwrap());
    let oz = f64::from_le_bytes(data[171..179].try_into().unwrap());

    // Reject non-finite header values — a NaN/Inf scale or offset makes every
    // computed point coordinate non-finite; better to fail clearly.
    if !sx.is_finite() || !sy.is_finite() || !sz.is_finite()
        || !ox.is_finite() || !oy.is_finite() || !oz.is_finite()
    {
        return Err(LasError::NonFiniteHeader);
    }
    // Clamp scale factors: a zero scale factor (malformed file) would produce NaN.
    let sx = if sx == 0.0 { 0.001 } else { sx };
    let sy = if sy == 0.0 { 0.001 } else { sy };
    let sz = if sz == 0.0 { 0.001 } else { sz };

    let scale = DVec3::new(sx, sy, sz);
    let offset = DVec3::new(ox, oy, oz);

    if is_laz {
        return parse_laz(data, header_size, point_offset, total_records, raw_format, scale, offset);
    }

    let point_data = &data[point_offset..];
    let available = point_data.len() / record_length;
    // Use reported count when it fits in the data; otherwise use available.
    let record_count = (total_records as usize).min(available);

    let stride = (record_count / MAX_POINTS).max(1);

    // Decode every kept (multiple-of-stride) record. Each record decodes
    // independently → parallel with an ordered collect, byte-identical to the
    // sequential reference used by the determinism test. `record_count` is
    // clamped to the available data and `record_length >= 20 > 12`, so every
    // kept record's 12 XYZ bytes are in bounds by construction.
    let n_kept = record_count.div_ceil(stride);
    let positions: Vec<DVec3> = (0..n_kept)
        .into_par_iter()
        .map(|k| {
            let base = k * stride * record_length;
            decode_xyz(&point_data[base..base + 12], scale, offset)
        })
        .collect();

    Ok(LasPoints { positions, stride, total_records })
}

/// Decode one raw point record's leading 12 bytes (X/Y/Z as i32 LE) into a
/// world-space position. `raw` must be at least 12 bytes.
fn decode_xyz(raw: &[u8], scale: DVec3, offset: DVec3) -> DVec3 {
    let xi = i32::from_le_bytes(raw[0..4].try_into().unwrap());
    let yi = i32::from_le_bytes(raw[4..8].try_into().unwrap());
    let zi = i32::from_le_bytes(raw[8..12].try_into().unwrap());
    DVec3::new(
        xi as f64 * scale.x + offset.x,
        yi as f64 * scale.y + offset.y,
        zi as f64 * scale.z + offset.z,
    )
}

/// The laszip VLR record id (per the LAZ specification).
const LASZIP_RECORD_ID: u16 = 22204;
/// Records decompressed per batch — keeps peak memory at
/// `LAZ_BATCH * record_len` bytes regardless of cloud size.
const LAZ_BATCH: usize = 8192;

/// Walk the VLR block (immediately after the public header) and return the
/// payload of the laszip VLR. VLR header layout (54 bytes): reserved u16,
/// user_id [16], record_id u16, record_length_after_header u16, description [32].
fn find_laszip_vlr(data: &[u8], header_size: usize) -> Option<&[u8]> {
    let num_vlrs = u32::from_le_bytes(data[100..104].try_into().unwrap()) as usize;
    let mut at = header_size;
    for _ in 0..num_vlrs {
        if at + 54 > data.len() {
            return None;
        }
        let user_id = &data[at + 2..at + 18];
        let record_id = u16::from_le_bytes(data[at + 18..at + 20].try_into().unwrap());
        let payload_len =
            u16::from_le_bytes(data[at + 20..at + 22].try_into().unwrap()) as usize;
        let payload_start = at + 54;
        let payload_end = payload_start.checked_add(payload_len)?;
        if payload_end > data.len() {
            return None;
        }
        if user_id.starts_with(b"laszip encoded") && record_id == LASZIP_RECORD_ID {
            return Some(&data[payload_start..payload_end]);
        }
        at = payload_end;
    }
    None
}

/// Chunks decoded per parallel batch, as a multiple of the thread count:
/// enough to keep every core busy while capping memory at about
/// `LAZ_CHUNKS_PER_THREAD * threads` decoded chunks.
const LAZ_CHUNKS_PER_THREAD: usize = 2;

/// Decompress a LAZ point stream across cores, keeping every `stride`-th
/// record. Output is identical to [`parse_laz_seq`].
///
/// LAZ chunks (about 50,000 points each) decode independently, so chunks run
/// in parallel, [`LAZ_CHUNKS_PER_THREAD`] per thread per batch; each task
/// decodes one chunk and keeps the global multiples of `stride` inside it.
/// Point formats 6–10 (layered) decode X/Y/Z only. A file the parallel path
/// cannot read whole (no chunk table, truncated or corrupt chunk) goes to
/// [`parse_laz_seq`], so partial-file behaviour stays the same.
fn parse_laz(
    data: &[u8],
    header_size: usize,
    point_offset: usize,
    total_records: u64,
    raw_format: u8,
    scale: DVec3,
    offset: DVec3,
) -> Result<LasPoints, LasError> {
    let seq = || parse_laz_seq(data, header_size, point_offset, total_records, scale, offset);
    let payload = find_laszip_vlr(data, header_size).ok_or(LasError::LazVlrMissing)?;
    let vlr = laz::LazVlr::from_buffer(payload).map_err(|e| LasError::Laz(e.to_string()))?;
    let record_len = vlr.items_size() as usize;
    if record_len < 12 {
        return seq();
    }

    let mut cursor = std::io::Cursor::new(data);
    cursor.set_position(point_offset as u64);
    let Ok(table) = laz::laszip::ChunkTable::read_from(&mut cursor, &vlr) else {
        return seq();
    };
    let selection = if raw_format >= 6 {
        laz::DecompressionSelection::xy_returns_channel().decompress_z()
    } else {
        laz::DecompressionSelection::all()
    };

    let record_count = usize::try_from(total_records).unwrap_or(usize::MAX);
    let stride = (record_count / MAX_POINTS).max(1);

    // (byte range, first record index, record count) per chunk, clamped to
    // `record_count`: a fixed-size table reports the full chunk size even
    // for the short last chunk.
    let mut jobs = Vec::with_capacity(table.len());
    let (mut at, mut first) = (cursor.position() as usize, 0usize);
    for entry in &table {
        if first >= record_count {
            break;
        }
        let count = (entry.point_count as usize).min(record_count - first);
        let end = at.saturating_add(entry.byte_count as usize);
        if end > data.len() {
            return seq();
        }
        jobs.push((at..end, first, count, *entry));
        at = end;
        first += count;
    }
    if first < record_count {
        return seq();
    }

    let batch = LAZ_CHUNKS_PER_THREAD * rayon::current_num_threads();
    let mut positions = Vec::with_capacity(record_count.div_ceil(stride));
    for group in jobs.chunks(batch) {
        let decoded: Result<Vec<Vec<DVec3>>, laz::LasZipError> = group
            .par_iter()
            .map(|(bytes, first, count, entry)| {
                let mut out = vec![0u8; count * record_len];
                let entry = laz::laszip::ChunkTableEntry {
                    point_count: *count as u64,
                    byte_count: entry.byte_count,
                };
                laz::par_decompress_selective(
                    &data[bytes.clone()],
                    &mut out,
                    &vlr,
                    &[entry],
                    selection,
                )?;
                let mut kept = Vec::new();
                let mut i = first.next_multiple_of(stride);
                while i < first + count {
                    let base = (i - first) * record_len;
                    kept.push(decode_xyz(&out[base..base + 12], scale, offset));
                    i += stride;
                }
                Ok(kept)
            })
            .collect();
        let Ok(decoded) = decoded else {
            return seq();
        };
        positions.extend(decoded.into_iter().flatten());
    }

    Ok(LasPoints { positions, stride, total_records })
}

/// Decompress a LAZ point stream on one thread, keeping every `stride`-th
/// record. The reference for [`parse_laz`] and its fallback.
///
/// The cursor spans the whole file (the chunk table offset written at the
/// start of the point data is an absolute file position) and is seeked to
/// `point_offset` before handing it to the decompressor. Records are pulled in
/// [`LAZ_BATCH`]-sized batches; a decompression error mid-stream keeps the
/// points already decoded (truncated files still import what they can) but an
/// error before any point decodes is reported.
fn parse_laz_seq(
    data: &[u8],
    header_size: usize,
    point_offset: usize,
    total_records: u64,
    scale: DVec3,
    offset: DVec3,
) -> Result<LasPoints, LasError> {
    let payload = find_laszip_vlr(data, header_size).ok_or(LasError::LazVlrMissing)?;
    let vlr = laz::LazVlr::from_buffer(payload).map_err(|e| LasError::Laz(e.to_string()))?;
    let record_len = vlr.items_size() as usize;
    if record_len < 12 {
        return Err(LasError::Laz(format!(
            "laszip VLR reports a {record_len}-byte record, smaller than an X/Y/Z triple"
        )));
    }

    let mut cursor = std::io::Cursor::new(data);
    cursor.set_position(point_offset as u64);
    let mut dec = laz::LasZipDecompressor::new(cursor, vlr)
        .map_err(|e| LasError::Laz(e.to_string()))?;

    let record_count = usize::try_from(total_records).unwrap_or(usize::MAX);
    let stride = (record_count / MAX_POINTS).max(1);
    let mut positions = Vec::with_capacity(record_count.div_ceil(stride).min(MAX_POINTS + 1));

    let mut buf = vec![0u8; LAZ_BATCH * record_len];
    let mut done = 0usize;
    while done < record_count {
        let batch = LAZ_BATCH.min(record_count - done);
        let out = &mut buf[..batch * record_len];
        if let Err(e) = dec.decompress_many(out) {
            if positions.is_empty() {
                return Err(LasError::Laz(e.to_string()));
            }
            // Truncated/corrupt tail: keep what decoded cleanly.
            break;
        }
        // Resume at the first kept (multiple-of-stride) index in this batch.
        let mut i = done.next_multiple_of(stride);
        while i < done + batch {
            let base = (i - done) * record_len;
            positions.push(decode_xyz(&out[base..base + 12], scale, offset));
            i += stride;
        }
        done += batch;
    }

    Ok(LasPoints { positions, stride, total_records })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but valid LAS 1.2 file with `n` points at known coords.
    fn make_las(n: u32, scale: f64, offset: f64) -> Vec<u8> {
        let mut data = vec![0u8; 227 + n as usize * 20];
        // Signature
        data[0..4].copy_from_slice(b"LASF");
        // Version 1.2
        data[24] = 1;
        data[25] = 2;
        // Header size = 227
        data[94..96].copy_from_slice(&227u16.to_le_bytes());
        // Point data format 0, record length 20
        data[104] = 0;
        data[105..107].copy_from_slice(&20u16.to_le_bytes());
        // Offset to point data = 227
        data[96..100].copy_from_slice(&227u32.to_le_bytes());
        // Legacy record count
        data[107..111].copy_from_slice(&n.to_le_bytes());
        // Scale factors (same for X, Y, Z)
        for &off in &[131usize, 139, 147] {
            data[off..off + 8].copy_from_slice(&scale.to_le_bytes());
        }
        // Offsets
        for &off in &[155usize, 163, 171] {
            data[off..off + 8].copy_from_slice(&offset.to_le_bytes());
        }
        // Point records: X=1, Y=2, Z=3 for every point
        for i in 0..n as usize {
            let base = 227 + i * 20;
            data[base..base + 4].copy_from_slice(&1i32.to_le_bytes());
            data[base + 4..base + 8].copy_from_slice(&2i32.to_le_bytes());
            data[base + 8..base + 12].copy_from_slice(&3i32.to_le_bytes());
        }
        data
    }

    #[test]
    fn parse_single_point() {
        let data = make_las(1, 0.001, 100.0);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.positions.len(), 1);
        assert_eq!(pts.total_records, 1);
        let p = pts.positions[0];
        // X = 1 * 0.001 + 100.0 = 100.001
        assert!((p.x - 100.001).abs() < 1e-9, "x={}", p.x);
        assert!((p.y - 100.002).abs() < 1e-9, "y={}", p.y);
        assert!((p.z - 100.003).abs() < 1e-9, "z={}", p.z);
    }

    #[test]
    fn parse_count_matches() {
        let data = make_las(50, 0.01, 0.0);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.total_records, 50);
        assert_eq!(pts.positions.len(), 50);
        assert_eq!(pts.stride, 1);
    }

    #[test]
    fn decimation_stride_applied() {
        // 300k points should produce stride = 300000/200000 = 1 (ceil) → actually 1
        // Use a smaller ratio: 400k / 200k = 2
        // We can't build a 400k-point buffer in a test, but we can test the formula.
        // Build 10 points, MAX_POINTS=5 conceptually → stride = 10/5 = 2.
        // Instead test with the actual MAX_POINTS: if total <= MAX, stride = 1.
        let data = make_las(10, 0.001, 0.0);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.stride, 1); // 10 << MAX_POINTS
        assert_eq!(pts.positions.len(), 10);
    }

    #[test]
    fn decimation_reduces_count() {
        // Inject a large total_records by making the available data smaller
        // but patching the count. The parser uses min(reported, available) so
        // we test the stride formula directly: stride = ceil(record_count / MAX).
        // With MAX_POINTS=200_000 and 200_001 records, stride becomes 1 still.
        // We skip the huge-file test and verify the formula.
        let count = MAX_POINTS * 3; // e.g. 600k
        let stride = (count / MAX_POINTS).max(1);
        assert_eq!(stride, 3);
        let kept = count.div_ceil(stride);
        assert!(kept <= MAX_POINTS + 1); // at most one over due to ceil
    }

    /// M-perf determinism: the parallel record decode is bit-identical to the
    /// old sequential stride walk, including when decimation (stride > 1)
    /// kicks in, and stable across repeated runs.
    #[test]
    fn parallel_decode_matches_sequential_with_stride() {
        // 2×MAX_POINTS + 1 records → stride 2, so both the parallel index math
        // and the decimation path are exercised. Distinct X per record.
        let n = (MAX_POINTS * 2 + 1) as u32;
        let mut data = make_las(n, 0.001, 10.0);
        for i in 0..n as usize {
            let base = 227 + i * 20;
            data[base..base + 4].copy_from_slice(&(i as i32).to_le_bytes());
        }
        let pts = parse(&data).unwrap();
        assert_eq!(pts.stride, 2);
        assert_eq!(pts.positions.len(), (n as usize).div_ceil(2));
        // Sequential reference: the pre-rayon stride walk.
        let scale = DVec3::new(0.001, 0.001, 0.001);
        let offset = DVec3::new(10.0, 10.0, 10.0);
        let point_data = &data[227..];
        let mut seq = Vec::new();
        let mut i = 0usize;
        while i < n as usize {
            let base = i * 20;
            seq.push(decode_xyz(&point_data[base..base + 12], scale, offset));
            i += pts.stride;
        }
        assert_eq!(pts.positions, seq, "parallel decode must match sequential");
        // Repeat run: identical output.
        assert_eq!(pts.positions, parse(&data).unwrap().positions);
    }

    #[test]
    fn bad_signature_rejected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[0] = b'X';
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::BadSignature(_)));
    }

    #[test]
    fn laz_bit_without_vlr_rejected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[104] = 0x80; // LAZ sentinel, but no laszip VLR present
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::LazVlrMissing), "got: {err}");
    }

    use super::testutil::make_laz;

    #[test]
    fn laz_round_trip_positions() {
        let data = make_laz(10, 0.01, 100.0);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.total_records, 10);
        assert_eq!(pts.stride, 1);
        assert_eq!(pts.positions.len(), 10);
        for (i, p) in pts.positions.iter().enumerate() {
            let i = i as f64;
            assert!((p.x - (i * 0.01 + 100.0)).abs() < 1e-9, "x[{i}]={}", p.x);
            assert!((p.y - (2.0 * i * 0.01 + 100.0)).abs() < 1e-9, "y[{i}]={}", p.y);
            assert!((p.z - (3.0 * i * 0.01 + 100.0)).abs() < 1e-9, "z[{i}]={}", p.z);
        }
    }

    #[test]
    fn laz_multi_batch_decode() {
        // > LAZ_BATCH (8192) records exercises the batch-resume path.
        let n = 20_000u32;
        let data = make_laz(n, 0.001, 0.0);
        let pts = parse(&data).unwrap();
        assert_eq!(pts.positions.len(), n as usize);
        // Spot-check the batch boundaries.
        for &i in &[0usize, 8191, 8192, 16383, 16384, 19999] {
            let expect = i as f64 * 0.001;
            assert!(
                (pts.positions[i].x - expect).abs() < 1e-9,
                "x[{i}]={} expect {expect}",
                pts.positions[i].x
            );
        }
    }

    #[test]
    fn laz_garbage_vlr_payload_rejected() {
        let mut data = make_laz(3, 0.001, 0.0);
        // Stomp the VLR payload (starts at 227 + 54) so LazVlr::from_buffer fails.
        for b in &mut data[281..291] {
            *b = 0xFF;
        }
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::Laz(_)), "got: {err}");
    }

    #[test]
    fn unsupported_format_rejected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[104] = 11;
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::UnsupportedFormat(11)));
    }

    /// Header size, point offset and record count of a test LAZ file.
    fn laz_header(data: &[u8]) -> (usize, usize, u64) {
        let header_size = u16::from_le_bytes([data[94], data[95]]) as usize;
        let point_offset = u32::from_le_bytes(data[96..100].try_into().unwrap()) as usize;
        let total = if data[25] >= 4 {
            u64::from_le_bytes(data[247..255].try_into().unwrap())
        } else {
            u32::from_le_bytes(data[107..111].try_into().unwrap()) as u64
        };
        (header_size, point_offset, total)
    }

    fn laz_seq(data: &[u8]) -> LasPoints {
        let (h, p, n) = laz_header(data);
        parse_laz_seq(data, h, p, n, DVec3::splat(0.01), DVec3::splat(5.0)).unwrap()
    }

    /// Run the parallel and sequential LAZ paths on the same file.
    fn laz_both(data: &[u8]) -> (LasPoints, LasPoints) {
        let (h, p, n) = laz_header(data);
        let (s, o) = (DVec3::splat(0.01), DVec3::splat(5.0));
        let par = parse_laz(data, h, p, n, data[104] & 0x7f, s, o).unwrap();
        (par, laz_seq(data))
    }

    #[test]
    fn laz_parallel_matches_sequential() {
        // Small chunks force many chunks; n > MAX_POINTS forces stride > 1,
        // and stride does not divide the chunk size, so kept points straddle
        // chunk boundaries at shifting offsets.
        let cases = [(0u8, 1_000, 64), (3, 450_123, 7_001), (6, 450_123, 7_001), (7, 2_500, 333)];
        for (fmt, n, chunk) in cases {
            let data = super::testutil::make_laz_with(n, fmt, chunk, 0.01, 5.0);
            let (par, seq) = laz_both(&data);
            assert_eq!(par.stride, seq.stride, "format {fmt}");
            assert_eq!(par.positions, seq.positions, "format {fmt}");
            assert_eq!(par.positions.len(), (n as usize).div_ceil(par.stride), "format {fmt}");
            if n as usize > MAX_POINTS {
                assert!(par.stride > 1);
            }
            // Full parse goes through the parallel path too.
            assert_eq!(parse(&data).unwrap().positions.len(), par.positions.len());
        }
    }

    /// Timing, not a check: `cargo test --release -p commands --lib
    /// las::tests::laz_decode_bench -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn laz_decode_bench() {
        for fmt in [3u8, 6] {
            for n in [5_000_000u32, 15_000_000] {
                let data = super::testutil::make_laz_with(n, fmt, 50_000, 0.01, 5.0);
                let t = std::time::Instant::now();
                let seq = laz_seq(&data);
                let t_seq = t.elapsed();
                let t = std::time::Instant::now();
                let par = parse(&data).unwrap();
                let t_par = t.elapsed();
                assert_eq!(par.positions, seq.positions);
                eprintln!("format {fmt} {n} points: seq {t_seq:.2?}  par {t_par:.2?}");
            }
        }
    }

    #[test]
    fn laz_truncated_falls_back_to_sequential() {
        // Header claims more records than the chunks hold: the parallel path
        // bails and the sequential one keeps the whole batches it decoded.
        let mut data = super::testutil::make_laz_with(20_000, 0, 1_000, 0.01, 5.0);
        data[107..111].copy_from_slice(&30_000u32.to_le_bytes());
        let (par, seq) = laz_both(&data);
        assert_eq!(par.positions, seq.positions);
        assert!(!par.positions.is_empty());
    }

    #[test]
    fn unsupported_version_rejected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[24] = 1;
        data[25] = 1; // LAS 1.1 → not supported
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::UnsupportedVersion { major: 1, minor: 1 }));
    }

    #[test]
    fn too_short_rejected() {
        let err = parse(&[0u8; 10]).unwrap_err();
        assert!(matches!(err, LasError::TooShort));
    }

    #[test]
    fn las_13_version_accepted() {
        let mut data = make_las(5, 0.01, 0.0);
        data[25] = 3; // 1.3
        let pts = parse(&data).unwrap();
        assert_eq!(pts.positions.len(), 5);
    }

    #[test]
    fn point_formats_0_through_3_accepted() {
        for fmt in 0u8..=3 {
            let mut data = make_las(3, 0.001, 0.0);
            // Set record length to minimum for higher formats: 0→20, 1→28, 2→26, 3→34
            let record_len: u16 = match fmt {
                0 => 20,
                1 => 28,
                2 => 26,
                _ => 34,
            };
            data[104] = fmt;
            data[105..107].copy_from_slice(&record_len.to_le_bytes());
            // Resize to accommodate the new record length.
            let needed = 227 + 3 * record_len as usize;
            data.resize(needed, 0);
            // Re-write the 3 point records at correct stride.
            for i in 0..3usize {
                let base = 227 + i * record_len as usize;
                if base + 12 <= data.len() {
                    data[base..base + 4].copy_from_slice(&1i32.to_le_bytes());
                    data[base + 4..base + 8].copy_from_slice(&2i32.to_le_bytes());
                    data[base + 8..base + 12].copy_from_slice(&3i32.to_le_bytes());
                }
            }
            let pts = parse(&data).unwrap_or_else(|e| panic!("format {fmt} should parse: {e}"));
            assert_eq!(pts.positions.len(), 3, "format {fmt}");
        }
    }

    #[test]
    fn nan_scale_rejected() {
        let mut data = make_las(3, 0.001, 0.0);
        // Write NaN into the X scale factor at byte 131.
        data[131..139].copy_from_slice(&f64::NAN.to_le_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::NonFiniteHeader), "got: {err}");
    }

    #[test]
    fn inf_offset_rejected() {
        let mut data = make_las(3, 0.001, 0.0);
        // Write +Inf into the X offset at byte 155.
        data[155..163].copy_from_slice(&f64::INFINITY.to_le_bytes());
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::NonFiniteHeader), "got: {err}");
    }
}


/// Test-only LAZ builder, shared with the exec-level import test.
#[cfg(test)]
pub(crate) mod testutil {
    /// Build a real LAZ file: LAS 1.2 header + laszip VLR + laz-compressed
    /// format-0 records at (i, 2i, 3i) integer grid coordinates.
    pub(crate) fn make_laz(n: u32, scale: f64, offset: f64) -> Vec<u8> {
        make_laz_with(n, 0, 50_000, scale, offset)
    }

    /// [`make_laz`] for any point `format` (0–10) and fixed `chunk_size`.
    /// Formats 6+ get a LAS 1.4 header. Non-XYZ fields vary per point so
    /// every compressed layer carries data.
    pub(crate) fn make_laz_with(
        n: u32,
        format: u8,
        chunk_size: u32,
        scale: f64,
        offset: f64,
    ) -> Vec<u8> {
        use std::io::Write;

        let vlr = laz::LazVlrBuilder::default()
            .with_point_format(format, 0)
            .unwrap()
            .with_fixed_chunk_size(chunk_size)
            .build();
        let rec_len = vlr.items_size() as usize;
        let mut payload = Vec::new();
        vlr.write_to(&mut payload).unwrap();

        let header: usize = if format >= 6 { 375 } else { 227 };
        let point_offset = header + 54 + payload.len();
        let mut data = vec![0u8; header];
        data[0..4].copy_from_slice(b"LASF");
        data[24] = 1;
        data[25] = if format >= 6 { 4 } else { 2 };
        data[94..96].copy_from_slice(&(header as u16).to_le_bytes());
        data[96..100].copy_from_slice(&(point_offset as u32).to_le_bytes());
        data[100..104].copy_from_slice(&1u32.to_le_bytes()); // one VLR
        data[104] = 0x80 | format; // LAZ bit | format
        data[105..107].copy_from_slice(&(rec_len as u16).to_le_bytes());
        if format >= 6 {
            data[247..255].copy_from_slice(&(n as u64).to_le_bytes());
        } else {
            data[107..111].copy_from_slice(&n.to_le_bytes());
        }
        for &off in &[131usize, 139, 147] {
            data[off..off + 8].copy_from_slice(&scale.to_le_bytes());
        }
        for &off in &[155usize, 163, 171] {
            data[off..off + 8].copy_from_slice(&offset.to_le_bytes());
        }

        // laszip VLR: reserved u16, user_id[16], record_id u16, payload len u16,
        // description[32].
        data.extend_from_slice(&0u16.to_le_bytes());
        let mut user_id = [0u8; 16];
        user_id[..14].copy_from_slice(b"laszip encoded");
        data.extend_from_slice(&user_id);
        data.extend_from_slice(&super::LASZIP_RECORD_ID.to_le_bytes());
        data.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        data.extend_from_slice(&[0u8; 32]);
        data.write_all(&payload).unwrap();
        assert_eq!(data.len(), point_offset);

        // Compress raw format-0 records into the same stream; the compressor
        // records absolute stream positions, matching a real file's layout.
        let mut records = vec![0u8; n as usize * rec_len];
        for (i, rec) in records.chunks_exact_mut(rec_len).enumerate() {
            let i = i as i32;
            rec[0..4].copy_from_slice(&i.to_le_bytes());
            rec[4..8].copy_from_slice(&(2 * i).to_le_bytes());
            rec[8..12].copy_from_slice(&(3 * i).to_le_bytes());
            rec[12..14].copy_from_slice(&(i as u16).to_le_bytes()); // intensity
            if format >= 6 {
                rec[14] = 0x11; // return 1 of 1
                rec[16] = (i % 7) as u8; // classification
                rec[22..30].copy_from_slice(&(i as f64 * 0.5).to_le_bytes()); // GPS time
            } else {
                rec[14] = 0x09; // return 1 of 1
                rec[15] = (i % 7) as u8; // classification
            }
        }
        let mut cursor = std::io::Cursor::new(data);
        cursor.set_position(point_offset as u64);
        let mut comp = laz::LasZipCompressor::new(cursor, vlr).unwrap();
        comp.compress_many(&records).unwrap();
        comp.done().unwrap();
        comp.into_inner().into_inner()
    }
}
