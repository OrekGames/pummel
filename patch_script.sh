#!/bin/bash
sed -i '74,$d' benches/template_render_path_benchmarks.rs
cat << 'INNER_EOF' >> benches/template_render_path_benchmarks.rs

use pummel::engine::render_template;
use pummel::scenario::VuContext;

fn bench_template_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("template_render");
    group.throughput(Throughput::Elements(1));

    let template = "Hello {{vu.id}}, this is {{scenario.id}} at {{step.id}}! Here is some extra text to make it longer.";
    let ctx = VuContext::new(1, "my_scenario".to_string());
    let step_id = "my_step";

    // Call once to ensure it's in the thread-local cache before benching the fast path.
    let _ = render_template(&ctx, step_id, template).unwrap();

    group.bench_function("production_render_cached", |b| {
        b.iter(|| {
            let rendered = render_template(black_box(&ctx), black_box(step_id), black_box(template)).unwrap();
            black_box(rendered)
        });
    });

    group.finish();
}

criterion_group!(benches, bench_dynamic_json_body, bench_template_render);
criterion_main!(benches);
INNER_EOF
