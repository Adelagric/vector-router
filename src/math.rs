//! Numeric core of the pipeline: validation/alignment, L2 norm, normalization.
//!
//! All operations work on `&[f32]` or `&mut [f32]`. No allocation on the hot
//! path (apart from the fallback copy on misalignment, already planned for by
//! the caller via the pool).

use std::borrow::Cow;

use crate::error::Error;
use crate::pool::AlignedBuffer;

/// Validates the size, then returns a `&[f32]` view either zero-copy (the
/// common case: `raw` is 4-byte aligned) or after copying into `scratch` if
/// `raw` is misaligned.
///
/// - `expected_dim`: number of expected `f32` values.
/// - `raw`: raw bytes from the protobuf payload.
/// - `scratch`: aligned buffer, used only on misalignment.
///
/// On success:
/// - `Cow::Borrowed(raw_as_f32)` for zero-copy.
/// - `Cow::Borrowed(scratch_as_f32)` after copy. The lifetime stays `'a`
///   thanks to the re-borrow of `scratch`.
pub fn validate_and_align<'a>(
    raw: &'a [u8],
    expected_dim: usize,
    scratch: &'a mut AlignedBuffer,
) -> Result<Cow<'a, [f32]>, Error> {
    // Checked multiplication: an absurdly large `expected_dim` must not
    // silently wrap.
    let expected_bytes = expected_dim.checked_mul(4).ok_or(Error::InvalidDim {
        expected: expected_dim,
        got: raw.len(),
    })?;

    if raw.len() != expected_bytes {
        return Err(Error::InvalidDim {
            expected: expected_bytes,
            got: raw.len(),
        });
    }

    // Length is OK. Only alignment remains to check.
    match bytemuck::try_cast_slice::<u8, f32>(raw) {
        Ok(slice) => Ok(Cow::Borrowed(slice)),
        Err(_) => {
            // Misalignment: copy once into the aligned scratch.
            // The `misaligned_copies_total` metric tracks this rate: above
            // 1 % in prod, it's a signal to investigate the producer side.
            metrics::counter!("misaligned_copies_total").increment(1);
            scratch.copy_from_slice(raw)?;
            let aligned = scratch.as_f32()?;
            Ok(Cow::Borrowed(aligned))
        }
    }
}

/// Computes `Σ xᵢ²`, rejecting vectors containing NaN/Inf.
///
/// Strategy: eight parallel accumulators to break the sequential dependency
/// chain of a scalar reduction. LLVM can then emit AVX2 `vmulps` + `vaddps`
/// with ILP, without reordering additions (strict IEEE 754 compatible, no
/// need for `-C fast-math`).
///
/// NaN/Inf detection: off the hot path. We exploit IEEE 754 propagation
/// (NaN/Inf propagate through `*` and `+`) and check the sum at the end.
/// If non-finite, a second pass (rare path) surfaces the error.
///
/// The squared norm is enough to decide whether to normalize (comparison
/// to `1 ± ε`); the square root is only taken if normalization is needed.
#[inline]
pub fn l2_norm_squared(v: &[f32]) -> Result<f32, Error> {
    let chunks = v.chunks_exact(8);
    let rem = chunks.remainder();
    let mut a = [0.0f32; 8];
    for c in chunks {
        a[0] += c[0] * c[0];
        a[1] += c[1] * c[1];
        a[2] += c[2] * c[2];
        a[3] += c[3] * c[3];
        a[4] += c[4] * c[4];
        a[5] += c[5] * c[5];
        a[6] += c[6] * c[6];
        a[7] += c[7] * c[7];
    }
    let mut sum = ((a[0] + a[1]) + (a[2] + a[3])) + ((a[4] + a[5]) + (a[6] + a[7]));
    for &x in rem {
        sum += x * x;
    }

    if sum.is_finite() {
        return Ok(sum);
    }
    // Rare path: NaN/Inf in input, or overflow on sum (pathological case).
    for &x in v {
        if !x.is_finite() {
            return Err(Error::InvalidNumeric);
        }
    }
    Err(Error::InvalidNumeric)
}

/// Normalizes `v` in place if needed. Returns `true` if a division actually
/// occurred, `false` if the vector was already close enough to unit norm
/// (|norm² - 1| ≤ 2×10⁻⁶) or is the zero vector (norm² = 0).
#[inline]
pub fn normalize_in_place(v: &mut [f32], norm_squared: f32) -> bool {
    const TOLERANCE: f32 = 2e-6;
    if (norm_squared - 1.0).abs() <= TOLERANCE {
        return false;
    }
    if norm_squared <= 0.0 {
        // Zero vector: no direction, leave as-is.
        return false;
    }
    let norm = norm_squared.sqrt();
    for x in v.iter_mut() {
        *x /= norm;
    }
    true
}

// --- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of(floats: &[f32]) -> Vec<u8> {
        bytemuck::cast_slice(floats).to_vec()
    }

    #[test]
    fn aligned_input_is_zero_copy() {
        // Source from a Vec<f32>: alignment guaranteed.
        let input = vec![1.0f32, 2.0, 3.0, 4.0];
        let raw: &[u8] = bytemuck::cast_slice(&input);
        let mut scratch = AlignedBuffer::new(64);

        let view = validate_and_align(raw, 4, &mut scratch).expect("alignement OK");
        assert_eq!(&*view, &[1.0, 2.0, 3.0, 4.0]);

        // View pointer == input bytes pointer → zero-copy.
        let view_ptr = view.as_ptr() as usize;
        let raw_ptr = raw.as_ptr() as usize;
        assert_eq!(
            view_ptr, raw_ptr,
            "slice retourné devrait pointer dans raw (zero-copy)"
        );
    }

    #[test]
    fn misaligned_input_is_copied_to_scratch() {
        // To guarantee reliable misalignment (miri does not honor stack
        // alignments by default), we store into a `[u32; 5]` whose base is
        // 4-aligned, then take &bytes[1..17]: necessarily not a multiple of 4.
        let floats = [1.0f32, 2.0, 3.0, 4.0];
        let raw_bytes = bytes_of(&floats); // 16 aligned bytes
        let mut backing = [0u32; 5]; // 20 bytes, 4-aligned guaranteed
        let shifted: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
        shifted[1..1 + raw_bytes.len()].copy_from_slice(&raw_bytes);
        let misaligned: &[u8] = &shifted[1..17];
        assert_eq!(misaligned.len(), 16);
        assert_ne!(misaligned.as_ptr() as usize % 4, 0, "désaligné attendu");

        let mut scratch = AlignedBuffer::new(64);
        let view = validate_and_align(misaligned, 4, &mut scratch).expect("copie OK");

        assert_eq!(&*view, &[1.0, 2.0, 3.0, 4.0]);
        // The view pointer is NOT inside the `misaligned` range.
        let vp = view.as_ptr() as usize;
        let mp = misaligned.as_ptr() as usize;
        assert!(
            vp < mp || vp >= mp + misaligned.len(),
            "la vue devrait pointer dans scratch, pas dans la source"
        );
    }

    #[test]
    fn wrong_size_is_invalid_dim() {
        let raw = vec![0u8; 15]; // 15 bytes for dim=4 (16 expected)
        let mut scratch = AlignedBuffer::new(64);
        let err = validate_and_align(&raw, 4, &mut scratch).expect_err("dim wrong");
        match err {
            Error::InvalidDim { expected, got } => {
                assert_eq!(expected, 16);
                assert_eq!(got, 15);
            }
            other => panic!("attendu InvalidDim, eu {other:?}"),
        }
    }

    #[test]
    fn oversize_is_invalid_dim() {
        let raw = vec![0u8; 24]; // 24 bytes for dim=4 (16 expected)
        let mut scratch = AlignedBuffer::new(64);
        let err = validate_and_align(&raw, 4, &mut scratch).expect_err("oversize");
        assert!(matches!(err, Error::InvalidDim { .. }));
    }

    #[test]
    fn l2_norm_normal_vector() {
        let v = [3.0f32, 4.0]; // norm = 5, norm² = 25
        let got = l2_norm_squared(&v).unwrap();
        assert!((got - 25.0).abs() < 1e-5);
    }

    #[test]
    fn l2_norm_rejects_nan() {
        let v = [1.0f32, f32::NAN, 3.0];
        assert!(matches!(l2_norm_squared(&v), Err(Error::InvalidNumeric)));
    }

    #[test]
    fn l2_norm_rejects_infinity() {
        let v = [1.0f32, f32::INFINITY, 3.0];
        assert!(matches!(l2_norm_squared(&v), Err(Error::InvalidNumeric)));
        let v = [1.0f32, f32::NEG_INFINITY, 3.0];
        assert!(matches!(l2_norm_squared(&v), Err(Error::InvalidNumeric)));
    }

    #[test]
    fn l2_norm_zero_vector() {
        let v = [0.0f32; 8];
        assert_eq!(l2_norm_squared(&v).unwrap(), 0.0);
    }

    #[test]
    fn normalize_skips_already_normalized() {
        // Unit vector aligned on one axis: norm² = exactly 1.
        let mut v = [1.0f32, 0.0, 0.0];
        let before = v;
        let changed = normalize_in_place(&mut v, 1.0);
        assert!(!changed);
        assert_eq!(v, before);
    }

    #[test]
    fn normalize_within_tolerance_skips() {
        let mut v = [0.5f32; 4]; // norm² = exactly 1.0 (4 × 0.25)
        let before = v;
        let changed = normalize_in_place(&mut v, 1.000_001);
        assert!(!changed);
        assert_eq!(v, before);
    }

    #[test]
    fn normalize_rescales_non_unit_vector() {
        let mut v = [3.0f32, 4.0];
        let n2 = l2_norm_squared(&v).unwrap();
        let changed = normalize_in_place(&mut v, n2);
        assert!(changed);
        let final_n2 = l2_norm_squared(&v).unwrap();
        assert!(
            (final_n2 - 1.0).abs() < 1e-6,
            "norme après normalisation doit être ≈ 1, eu {final_n2}"
        );
    }

    #[test]
    fn normalize_zero_vector_is_noop() {
        let mut v = [0.0f32; 4];
        let changed = normalize_in_place(&mut v, 0.0);
        assert!(!changed);
        assert_eq!(v, [0.0; 4]);
    }

    #[test]
    fn pipeline_end_to_end() {
        // Typical scenario: bytes aligned → norm² → normalize → check.
        let floats = vec![0.0f32, 3.0, 4.0, 0.0]; // norm = 5
        let raw = bytes_of(&floats);
        let mut scratch = AlignedBuffer::new(64);

        let view = validate_and_align(&raw, 4, &mut scratch).unwrap();
        let n2 = l2_norm_squared(&view).unwrap();
        assert!((n2 - 25.0).abs() < 1e-5);

        // To normalize, we need a mutable buffer. In the real pipeline we
        // copy the view into an output buffer. Here we test the function on
        // a local vec.
        let mut owned: Vec<f32> = view.to_vec();
        drop(view);
        let changed = normalize_in_place(&mut owned, n2);
        assert!(changed);
        let final_n2 = l2_norm_squared(&owned).unwrap();
        assert!((final_n2 - 1.0).abs() < 1e-6);
    }
}
