//! Benches de `l2_norm_squared` et `normalize_in_place`.
//!
//! Règles respectées :
//! - `black_box` sur tous les inputs ET outputs.
//! - assertion de sanité vérifiant que le résultat dépend bien de l'input
//!   (évite de constater tardivement qu'un `black_box` manquant a laissé
//!   le compilateur constant-folder).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use vector_router::math::{l2_norm_squared, normalize_in_place};

const DIM: usize = 1536;
const SIZES: &[usize] = &[256, 768, 1536, 3072];

fn gen_vector_sized(dim: usize, seed: u32) -> Vec<f32> {
    (0..dim)
        .map(|i| ((i as u32).wrapping_mul(seed) as f32) * 1e-3)
        .collect()
}

fn gen_vector(seed: u32) -> Vec<f32> {
    gen_vector_sized(DIM, seed)
}

fn bench_l2_norm_squared(c: &mut Criterion) {
    // Sanité globale : deux vecteurs différents → normes² différentes.
    let v1 = gen_vector(1);
    let v2 = gen_vector(2);
    let n1 = l2_norm_squared(&v1).expect("vecteur valide");
    let n2 = l2_norm_squared(&v2).expect("vecteur valide");
    assert!(
        (n1 - n2).abs() > 1.0,
        "sanité : deux vecteurs distincts doivent donner des normes² distinctes (n1={n1}, n2={n2})"
    );

    let mut group = c.benchmark_group("l2_norm_squared");
    for &dim in SIZES {
        let v = gen_vector_sized(dim, 1);
        group.throughput(Throughput::Elements(dim as u64));
        group.bench_with_input(BenchmarkId::from_parameter(dim), &v, |b, v| {
            b.iter(|| {
                let input = black_box(v.as_slice());
                let out = l2_norm_squared(input).expect("vecteur valide");
                black_box(out)
            });
        });
    }
    group.finish();
}

fn bench_normalize_in_place(c: &mut Criterion) {
    // On prépare un vecteur non normalisé ; sa copie est faite à chaque
    // itération via `iter_batched` pour ne pas mesurer une dérive
    // cumulative de la valeur.
    let template = gen_vector(3);
    let n2 = l2_norm_squared(&template).expect("valide");

    // Sanité : la fonction doit bien modifier le vecteur.
    let mut probe = template.clone();
    let changed = normalize_in_place(&mut probe, n2);
    assert!(
        changed,
        "sanité : un vecteur non unitaire doit être normalisé"
    );
    let final_n2 = l2_norm_squared(&probe).unwrap();
    assert!(
        (final_n2 - 1.0).abs() < 1e-4,
        "sanité : post-normalisation la norme² doit être ≈ 1 (eu {final_n2})"
    );

    c.bench_function("normalize_in_place_1536", |b| {
        b.iter_batched(
            || template.clone(),
            |mut input| {
                let n2 = black_box(n2);
                let input_ref = black_box(input.as_mut_slice());
                let changed = normalize_in_place(input_ref, n2);
                black_box(changed);
                black_box(input)
            },
            criterion::BatchSize::SmallInput,
        );
    });
}

criterion_group!(benches, bench_l2_norm_squared, bench_normalize_in_place);
criterion_main!(benches);
