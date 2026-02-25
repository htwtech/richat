use criterion::{criterion_group, criterion_main};

mod transaction;

criterion_group!(benches, transaction::bench_bloom_prefilter);

criterion_main!(benches);
