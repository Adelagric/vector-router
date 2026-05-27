//! Bench de `validate_and_align` sur deux scénarios : input aligné (zero-copy)
//! et input désaligné (copie dans scratch).

use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};

use vector_router::math::validate_and_align;
use vector_router::pool::AlignedBuffer;

const DIM: usize = 1536;
const BYTES: usize = DIM * 4;

fn gen_floats() -> Vec<f32> {
    (0..DIM).map(|i| (i as f32) * 1e-3).collect()
}

fn bench_aligned(c: &mut Criterion) {
    let floats = gen_floats();
    let raw: Vec<u8> = bytemuck::cast_slice(&floats).to_vec();
    assert_eq!(raw.len(), BYTES);

    // Sanité : la vue doit bien refléter les floats d'origine.
    let mut probe_scratch = AlignedBuffer::new(BYTES);
    let probe = validate_and_align(&raw, DIM, &mut probe_scratch).unwrap();
    assert_eq!(probe[0], 0.0);
    assert!((probe[1] - 0.001).abs() < 1e-6);

    c.bench_function("validate_and_align_aligned_1536", |b| {
        let mut scratch = AlignedBuffer::new(BYTES);
        b.iter(|| {
            let input = black_box(raw.as_slice());
            let view = validate_and_align(input, DIM, &mut scratch).expect("OK");
            black_box(view.len())
        });
    });
}

fn bench_misaligned(c: &mut Criterion) {
    let floats = gen_floats();
    let raw_bytes: Vec<u8> = bytemuck::cast_slice(&floats).to_vec();
    // Backing u32 pour garantir une base 4-alignée, puis offset de 1 octet.
    let mut backing: Vec<u32> = vec![0; (BYTES + 4).div_ceil(4)];
    let shifted: &mut [u8] = bytemuck::cast_slice_mut(&mut backing);
    shifted[1..1 + BYTES].copy_from_slice(&raw_bytes);

    // Sanité : vérifie qu'un appel réussit et que la vue est correcte.
    let mut probe_scratch = AlignedBuffer::new(BYTES);
    let view = validate_and_align(&shifted[1..1 + BYTES], DIM, &mut probe_scratch).unwrap();
    assert_eq!(view.len(), DIM);

    c.bench_function("validate_and_align_misaligned_1536", |b| {
        let mut scratch = AlignedBuffer::new(BYTES);
        b.iter(|| {
            let input = black_box(&shifted[1..1 + BYTES]);
            let view = validate_and_align(input, DIM, &mut scratch).expect("OK");
            black_box(view.len())
        });
    });
}

criterion_group!(benches, bench_aligned, bench_misaligned);
criterion_main!(benches);
