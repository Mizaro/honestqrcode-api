use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use honestqr_core::{QrData, QrFormat, QrSpec, RenderOptions, render};

fn render_benchmarks(criterion: &mut Criterion) {
    let payloads = [
        ("short", "https://honestqrcode.com/".to_owned()),
        (
            "medium",
            "https://honestqrcode.com/api?value=benchmark&".repeat(8),
        ),
        ("large", "Honest QR Code benchmark payload ".repeat(50)),
    ];
    let mut group = criterion.benchmark_group("render");
    for (name, payload) in payloads {
        group.throughput(Throughput::Bytes(payload.len() as u64));
        for format in [QrFormat::Svg, QrFormat::Png] {
            let spec = QrSpec {
                data: QrData::Text {
                    value: payload.clone(),
                },
                render: RenderOptions {
                    format,
                    ..RenderOptions::default()
                },
            };
            group.bench_with_input(
                BenchmarkId::new(format!("{format:?}"), name),
                &spec,
                |bencher, spec| bencher.iter(|| render(spec).expect("render")),
            );
        }
    }
    group.finish();
}

criterion_group!(benches, render_benchmarks);
criterion_main!(benches);
