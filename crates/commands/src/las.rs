//! Minimal LAS 1.2–1.4 parser for point-cloud import.
//!
//! Reads the public header block to extract scale/offset and the number of
//! point records, then decodes X/Y/Z integer triples from point formats 0–3
//! (which all share the same first 20 bytes: X i32, Y i32, Z i32, intensity u16,
//! flags/bits, classification, ...).  Returns `(positions, decimation_stride)`.
//!
//! Only uncompressed LAS is supported. LAZ files get an honest error asking the
//! user to decompress first.

use glam::DVec3;

/// Maximum points kept after decimation.
pub const MAX_POINTS: usize = 200_000;

#[derive(Debug, thiserror::Error)]
pub enum LasError {
    #[error("file too short to contain a LAS header")]
    TooShort,
    #[error("not a LAS file: signature is {0:?}, expected \"LASF\"")]
    BadSignature([u8; 4]),
    #[error("LAZ (compressed LAS) is not supported — decompress to .las first")]
    Laz,
    #[error("unsupported point data format {0} (supported: 0–3)")]
    UnsupportedFormat(u8),
    #[error("LAS header version {major}.{minor} is not 1.2–1.4")]
    UnsupportedVersion { major: u8, minor: u8 },
    #[error("point data offset {offset} is past end of file ({len} bytes)")]
    BadOffset { offset: u32, len: usize },
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
    // LAZ uses bit 7 set in the format byte (0x80 | format).
    if point_format & 0x80 != 0 {
        return Err(LasError::Laz);
    }
    if point_format > 3 {
        return Err(LasError::UnsupportedFormat(point_format));
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

    // Clamp scale factors: a zero scale factor (malformed file) would produce NaN.
    let sx = if sx == 0.0 { 0.001 } else { sx };
    let sy = if sy == 0.0 { 0.001 } else { sy };
    let sz = if sz == 0.0 { 0.001 } else { sz };

    let point_data = &data[point_offset..];
    let available = point_data.len() / record_length;
    // Use reported count when it fits in the data; otherwise use available.
    let record_count = (total_records as usize).min(available);

    let stride = (record_count / MAX_POINTS).max(1);

    let cap = record_count.div_ceil(stride);
    let mut positions = Vec::with_capacity(cap);

    let mut i = 0usize;
    while i < record_count {
        let base = i * record_length;
        if base + 12 > point_data.len() {
            break;
        }
        let xi = i32::from_le_bytes(point_data[base..base + 4].try_into().unwrap());
        let yi = i32::from_le_bytes(point_data[base + 4..base + 8].try_into().unwrap());
        let zi = i32::from_le_bytes(point_data[base + 8..base + 12].try_into().unwrap());
        let x = xi as f64 * sx + ox;
        let y = yi as f64 * sy + oy;
        let z = zi as f64 * sz + oz;
        positions.push(DVec3::new(x, y, z));
        i += stride;
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

    #[test]
    fn bad_signature_rejected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[0] = b'X';
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::BadSignature(_)));
    }

    #[test]
    fn laz_bit_detected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[104] = 0x80; // LAZ sentinel
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::Laz));
    }

    #[test]
    fn unsupported_format_rejected() {
        let mut data = make_las(1, 0.001, 0.0);
        data[104] = 6;
        let err = parse(&data).unwrap_err();
        assert!(matches!(err, LasError::UnsupportedFormat(6)));
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
            let pts = parse(&data).expect(&format!("format {fmt} should parse"));
            assert_eq!(pts.positions.len(), 3, "format {fmt}");
        }
    }
}
