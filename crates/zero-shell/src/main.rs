//! Zero reference shell — a minimal embedder of `zero-engine`.
//!
//! Two modes, both thin wrappers over `engine.render_page(...)`:
//!   * default: open a live OS window with an address bar (see [`app`]).
//!   * `--png [out]`: headless — render once and write a PNG.
//!
//! Another developer could swap this shell for their own UI — the engine doesn't care.
//! The target may be a local file OR an http(s) URL (fetched over TLS).
//!
//! Usage:
//!   zero [target] [css]              # window   (target = file or URL)
//!   zero --png [target] [css] [out]  # headless PNG
//!   zero --ai [target]               # headless assistant report

mod ai;
mod app;
mod blocker;
mod fonts;
mod net;

use ai::Assistant;
use net::{is_url, load_target, ShellLoader};
use std::fs;
use zero_engine::Engine;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let png_mode = args.first().map(|a| a == "--png").unwrap_or(false);
    let ai_mode = args.first().map(|a| a == "--ai").unwrap_or(false);
    if png_mode || ai_mode {
        args.remove(0);
    }
    let mut args = args.into_iter().peekable();

    let (html, css, address) = match args.next() {
        None => {
            let html = fs::read_to_string("examples/test.html").expect("read html");
            let css = fs::read_to_string("examples/test.css").expect("read css");
            (html, css, "examples/test.html".to_string())
        }
        Some(target) if is_url(&target) => {
            let fetched = load_target(&target);
            (fetched.body, String::new(), fetched.url)
        }
        Some(target) => {
            let html = fs::read_to_string(&target).expect("could not read HTML file");
            // A following `*.css` arg is an explicit stylesheet; otherwise rely on
            // the page's own <style>. (Anything else is left for the PNG out path.)
            let css = if args.peek().map(|a| a.ends_with(".css")).unwrap_or(false) {
                fs::read_to_string(args.next().unwrap()).expect("read css")
            } else {
                String::new()
            };
            (html, css, target)
        }
    };

    if ai_mode {
        let doc = zero_engine::Document::load(&html, &css);
        let ctx = ai::PageContext {
            url: address,
            text: doc.page_text(),
            headings: doc.headings(),
            blocked_trackers: 0,
            secure: true,
        };
        let assistant = ai::LocalAssistant;
        println!("{}", assistant.respond(&ctx));
        println!("
[{}]", assistant.provenance());
        return;
    }

    let engine = fonts::build_engine();

    if png_mode {
        let out_path = args.next().unwrap_or_else(|| "output.png".into());
        render_to_png(&engine, &html, &css, &out_path, &address);
    } else {
        app::run_window(engine, html, css, address);
    }
}

fn render_to_png(engine: &Engine, html: &str, css: &str, out_path: &str, base: &str) {
    let loader = ShellLoader::new(base.to_string());
    let page = engine.render_page(html, css, 800.0, 600.0, &loader);
    for line in &page.console {
        eprintln!("[js] {line}");
    }
    let canvas = page.canvas;
    let buffer: Vec<u8> = canvas.pixels.iter().flat_map(|c| [c.r, c.g, c.b, c.a]).collect();
    let img = image::RgbaImage::from_raw(canvas.width as u32, canvas.height as u32, buffer)
        .expect("pixel buffer size mismatch");
    img.save(out_path).expect("could not write PNG");
    println!("Rendered -> {out_path} ({}x{})", canvas.width, canvas.height);
}
