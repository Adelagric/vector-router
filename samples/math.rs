// Extrait public du projet vector-router — fichier fourni à titre de vitrine
// technique. Ce fichier ne se compile pas seul ; il dépend du crate complet.
// Code complet distribué commercialement (Tier 3). Contact : kaleche@gmail.com
//
// (C) 2026 Adel Kaleche. All rights reserved. Voir LICENSE à la racine du repo.

//! Noyau numérique du pipeline : validation/alignement, norme L2, normalisation.
//!
//! Toutes les opérations manipulent des `&[f32]` ou `&mut [f32]`. Aucune
//! allocation dans le chemin chaud (hors copie de fallback en cas de
//! désalignement, déjà prévue par le caller via le pool).

use std::borrow::Cow;

use crate::error::Error;
use crate::pool::AlignedBuffer;

/// Valide la taille, puis retourne une vue `&[f32]` soit par zero-copy (cas
/// majoritaire : le `raw` est aligné sur 4 octets), soit après copie dans
/// `scratch` si `raw` est désaligné.
///
/// - `expected_dim` : nombre de `f32` attendus.
/// - `raw` : octets bruts du payload protobuf.
/// - `scratch` : buffer aligné, utilisé uniquement en cas de désalignement.
///
/// En cas de succès :
/// - `Cow::Borrowed(raw_as_f32)` si zero-copy.
/// - `Cow::Borrowed(scratch_as_f32)` si copie. Le lifetime reste `'a` grâce
///   au re-borrow de `scratch`.
pub fn validate_and_align<'a>(
    raw: &'a [u8],
    expected_dim: usize,
    scratch: &'a mut AlignedBuffer,
) -> Result<Cow<'a, [f32]>, Error> {
    // Multiplication checked : un `expected_dim` absurdement grand ne doit
    // pas wraper silencieusement.
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

    // La longueur est OK. Reste uniquement l'alignement à tester.
    match bytemuck::try_cast_slice::<u8, f32>(raw) {
        Ok(slice) => Ok(Cow::Borrowed(slice)),
        Err(_) => {
            // Désalignement : on copie une fois dans le scratch aligné.
            // La métrique `misaligned_copies_total` trace ce taux : au-delà
            // de 1 % en prod, c'est un signal à investiguer côté producteur.
            metrics::counter!("misaligned_copies_total").increment(1);
            scratch.copy_from_slice(raw)?;
            let aligned = scratch.as_f32()?;
            Ok(Cow::Borrowed(aligned))
        }
    }
}

/// Calcule `Σ xᵢ²` en rejetant les vecteurs contenant des NaN/Inf.
///
/// Stratégie : huit accumulateurs parallèles pour briser la chaîne de
/// dépendance séquentielle d'une réduction scalaire. LLVM peut alors générer
/// AVX2 `vmulps` + `vaddps` avec ILP, sans réordonner les additions (compatible
/// IEEE 754 strict, pas besoin de `-C fast-math`).
///
/// Détection NaN/Inf : hors du hot path. On exploite la propagation IEEE 754
/// (NaN/Inf se propagent à travers `*` et `+`), et on vérifie la somme en
/// sortie. Si non finie, un second passage (rare path) remonte l'erreur.
///
/// La norme au carré suffit pour décider d'une normalisation (comparaison
/// à `1 ± ε`) ; la racine n'est tirée que si la normalisation est nécessaire.
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
    // Rare path : NaN/Inf en entrée, ou overflow sur sum (cas pathologique).
    for &x in v {
        if !x.is_finite() {
            return Err(Error::InvalidNumeric);
        }
    }
    Err(Error::InvalidNumeric)
}

/// Normalise `v` in-place si nécessaire. Retourne `true` si une division a
/// effectivement eu lieu, `false` si le vecteur était déjà suffisamment proche
/// de la norme unité (|norm² - 1| ≤ 2×10⁻⁶) ou s'il est nul (norm² = 0).
#[inline]
pub fn normalize_in_place(v: &mut [f32], norm_squared: f32) -> bool {
    const TOLERANCE: f32 = 2e-6;
    if (norm_squared - 1.0).abs() <= TOLERANCE {
        return false;
    }
    if norm_squared <= 0.0 {
        // Vecteur nul : pas de direction, on laisse tel quel.
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
        // Source venant d'un Vec<f32> : alignement garanti.
        let input = vec![1.0f32, 2.0, 3.0, 4.0];
        let raw: &[u8] = bytemuck::cast_slice(&input);
        let mut scratch = AlignedBuffer::new(64);

        let view = validate_and_align(raw, 4, &mut scratch).expect("alignement OK");
        assert_eq!(&*view, &[1.0, 2.0, 3.0, 4.0]);

        // Pointeur de la vue == pointeur des octets d'entrée → zero-copy.
        let view_ptr = view.as_ptr() as usize;
        let raw_ptr = raw.as_ptr() as usize;
        assert_eq!(
            view_ptr, raw_ptr,
            "slice retourné devrait pointer dans raw (zero-copy)"
        );
    }

    #[test]
    fn misaligned_input_is_copied_to_scratch() {
        // Pour garantir un désalignement fiable (miri ne respecte pas les
        // alignements stack par défaut), on stocke dans un `[u32; 5]` dont
        // la base est 4-alignée, puis on prend &bytes[1..17] : forcément
        // non multiple de 4.
        let floats = [1.0f32, 2.0, 3.0, 4.0];
        let raw_bytes = bytes_of(&floats); // 16 octets alignés
        let mut backing = [0u32; 5]; // 20 octets, 4-aligné garanti
        let shifted: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
        shifted[1..1 + raw_bytes.len()].copy_from_slice(&raw_bytes);
        let misaligned: &[u8] = &shifted[1..17];
        assert_eq!(misaligned.len(), 16);
        assert_ne!(misaligned.as_ptr() as usize % 4, 0, "désaligné attendu");

        let mut scratch = AlignedBuffer::new(64);
        let view = validate_and_align(misaligned, 4, &mut scratch).expect("copie OK");

        assert_eq!(&*view, &[1.0, 2.0, 3.0, 4.0]);
        // Le pointeur de la vue N'EST PAS dans la plage de `misaligned`.
        let vp = view.as_ptr() as usize;
        let mp = misaligned.as_ptr() as usize;
        assert!(
            vp < mp || vp >= mp + misaligned.len(),
            "la vue devrait pointer dans scratch, pas dans la source"
        );
    }

    #[test]
    fn wrong_size_is_invalid_dim() {
        let raw = vec![0u8; 15]; // 15 octets pour dim=4 (16 attendus)
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
        let raw = vec![0u8; 24]; // 24 octets pour dim=4 (16 attendus)
        let mut scratch = AlignedBuffer::new(64);
        let err = validate_and_align(&raw, 4, &mut scratch).expect_err("oversize");
        assert!(matches!(err, Error::InvalidDim { .. }));
    }

    #[test]
    fn l2_norm_normal_vector() {
        let v = [3.0f32, 4.0]; // norme = 5, norme² = 25
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
        // Vecteur unitaire aligné sur un axe : norme² = 1 exactement.
        let mut v = [1.0f32, 0.0, 0.0];
        let before = v;
        let changed = normalize_in_place(&mut v, 1.0);
        assert!(!changed);
        assert_eq!(v, before);
    }

    #[test]
    fn normalize_within_tolerance_skips() {
        let mut v = [0.5f32; 4]; // norme² = 1.0 exactement (4 × 0.25)
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
        // Scénario type : bytes aligned → norme² → normalize → vérif.
        let floats = vec![0.0f32, 3.0, 4.0, 0.0]; // norme = 5
        let raw = bytes_of(&floats);
        let mut scratch = AlignedBuffer::new(64);

        let view = validate_and_align(&raw, 4, &mut scratch).unwrap();
        let n2 = l2_norm_squared(&view).unwrap();
        assert!((n2 - 25.0).abs() < 1e-5);

        // Pour normaliser, on a besoin d'un buffer mutable. Dans le vrai
        // pipeline, on copiera la vue dans un buffer de sortie. Ici, on teste
        // la fonction sur un vec local.
        let mut owned: Vec<f32> = view.to_vec();
        drop(view);
        let changed = normalize_in_place(&mut owned, n2);
        assert!(changed);
        let final_n2 = l2_norm_squared(&owned).unwrap();
        assert!((final_n2 - 1.0).abs() < 1e-6);
    }
}
