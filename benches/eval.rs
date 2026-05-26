use criterion::{criterion_group, criterion_main, Criterion};

fn bench_map_10k(c: &mut Criterion) {
    // Benchmark: map over a 10k-element range with increment function
    // [take 10000 [map [fn [let x] [+ x 1]] [range 0]]]
    c.bench_function("map_10k", |b| {
        b.iter(|| {
            tinct::eval_source("[take 10000 [map [fn [let x] [+ x 1]] [range 0]]]")
                .expect("eval failed");
        })
    });
}

fn bench_dict_1k(c: &mut Criterion) {
    // Benchmark: create a 1000-entry dict via repeated merge
    // [reduce [fn [let acc i] [merge acc [i: i]]] [] [range 1000]]
    c.bench_function("dict_1k", |b| {
        b.iter(|| {
            tinct::eval_source("[reduce [fn [let acc i] [merge acc [i: i]]] [] [range 1000]]")
                .expect("eval failed");
        })
    });
}

fn bench_deep_scope(c: &mut Criterion) {
    // Benchmark: deeply nested scopes (100 levels of let-bindings)
    // Measures Environment::get traversal on deeply nested scopes
    let mut expr = String::from("[x: 42]");
    for i in 0..100 {
        expr = format!("[outer{}: [inner{}: {}]]", i, i, expr);
    }
    expr.push_str(".outer0.inner0.x");

    c.bench_function("deep_scope", |b| {
        b.iter(|| {
            tinct::eval_source(&expr).expect("eval failed");
        })
    });
}

criterion_group!(benches, bench_map_10k, bench_dict_1k, bench_deep_scope);
criterion_main!(benches);
