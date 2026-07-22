//! What the script engine costs, in milliseconds.
//!
//! Run with `cargo run --release -p zero-engine --example jsbench`. The numbers
//! are the reason the interpreter is shaped the way it is — see the note on
//! `NameHasher`, and on why a loop body is borrowed rather than cloned.

fn main() {
    let cases: &[(&str, &str)] = &[
        ("loop+arith", "var s=0; for (var i=0;i<300000;i++) { s = s + i*2 - 1; } console.log(s);"),
        ("calls", "function f(a,b){ return a+b; } var s=0; for (var i=0;i<100000;i++) { s=f(s,i); } console.log(s);"),
        ("property", "var o={n:0}; for (var i=0;i<200000;i++) { o.n = o.n + 1; } console.log(o.n);"),
        ("array", "var a=[]; for (var i=0;i<50000;i++) { a.push(i); } var s=0; for (var i=0;i<50000;i++) { s+=a[i]; } console.log(s);"),
        ("string", "var s=''; for (var i=0;i<20000;i++) { s = s + 'x'; } console.log(s.length);"),
    ];
    for (name, source) in cases {
        let start = std::time::Instant::now();
        let out = zero_engine::js::run(source);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        // The result is printed too: a benchmark that computes the wrong answer
        // quickly is not a benchmark.
        println!("{name:12} {ms:8.1} ms   {:?}", out.console.first());
        assert!(out.errors.is_empty(), "{}: {:?}", name, out.errors);
    }
}
