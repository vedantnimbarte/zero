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
//!   zero --history                   # print saved browsing history

mod ai;
mod app;
mod blocker;
mod cookies;
mod crypto;
mod fonts;
mod i18n;
mod internal;
mod localstore;
mod net;
mod renderer;
mod sandbox;
mod settings;
mod spaces;
mod storage;
mod sync;
mod wire;

use ai::Assistant;
use net::{is_url, load_target, ShellLoader};
use std::fs;

fn main() {
    // Before a byte of anything a website sent is parsed: narrow what this
    // process is permitted to do, permanently.
    sandbox::harden();

    // Say so when profile data is not encrypted, rather than leaving the user to
    // assume it is — see crypto's note on which platforms have a backend.
    if !crypto::is_available() {
        eprintln!("note: no data-protection backend on this platform; profile data is stored in the clear");
    }
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Started as a renderer: answer one request from the pipe and exit. This is
    // never reached in the browser process.
    if args.first().map(|a| a == "--render-worker").unwrap_or(false) {
        renderer::serve();
        return;
    }
    if args.first().map(|a| a == "--history").unwrap_or(false) {
        for visit in storage::load_history() {
            println!("{}	{}	{}", visit.when, visit.url, visit.title);
        }
        return;
    }
    // Sync is a file, not a service: one sealed bundle, and a code to open it.
    match args.first().map(String::as_str) {
        Some("--export") => {
            let path = args
                .get(1)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(sync::default_path);
            match sync::export(&path) {
                Ok(code) => println!(
                    "Exported this space to {}\n\nCode: {code}\n\n\
                     Keep it: the bundle cannot be read without it, and it is not \
                     stored anywhere.",
                    path.display()
                ),
                Err(why) => eprintln!("export failed: {why}"),
            }
            return;
        }
        Some("--import") => {
            let path = args
                .get(1)
                .map(std::path::PathBuf::from)
                .unwrap_or_else(sync::default_path);
            let code = match args.get(2) {
                Some(code) => code.clone(),
                None => {
                    eprintln!("usage: zero --import <file> <code>");
                    return;
                }
            };
            match sync::import(&path, &code) {
                Ok(1) => println!("Restored 1 file into this space."),
                Ok(count) => println!("Restored {count} files into this space."),
                Err(why) => eprintln!("import failed: {why}"),
            }
            return;
        }
        _ => {}
    }
    let png_mode = args.first().map(|a| a == "--png").unwrap_or(false);
    // Like --png, but captures the whole browser window instead of just the page.
    let shot_mode = args.first().map(|a| a == "--shot").unwrap_or(false);
    let ai_mode = args.first().map(|a| a == "--ai").unwrap_or(false);
    if png_mode || ai_mode || shot_mode {
        args.remove(0);
    }
    let mut args = args.into_iter().peekable();

    let mut restore_session = false;
    let (html, css, address) = match args.next() {
        None => {
            restore_session = true;
            let html = fs::read_to_string("examples/test.html").expect("read html");
            let css = fs::read_to_string("examples/test.css").expect("read css");
            (html, css, "examples/test.html".to_string())
        }
        Some(target) if is_url(&target) || internal::is_internal(&target) => {
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
        let doc = zero_engine::Document::load_with(&html, &css, std::rc::Rc::new(ShellLoader::new(address.clone())));
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

    if shot_mode {
        let out_path = args.next().unwrap_or_else(|| "window.png".into());
        // A trailing WxH exercises breakpoints: sites switch layout on the
        // *content* width, which is the window minus Zero's own chrome.
        let mut rest: Vec<String> = args.collect();
        // A leading WxH exercises breakpoints; anything after it poses the
        // chrome (`menu`, `hover:star`, `layout=horizontal`, `tabs:4`, …) so
        // every surface can be reviewed without holding the mouse still.
        let (w, h) = match rest.first().and_then(|size| size.split_once(['x', 'X'])) {
            Some((w, h)) => match (w.parse(), h.parse()) {
                (Ok(w), Ok(h)) => {
                    rest.remove(0);
                    (w, h)
                }
                _ => (1280, 800),
            },
            None => (1280, 800),
        };
        let (pixels, w, h) = app::screenshot(engine, html, css, address, w, h, &rest);
        let buffer: Vec<u8> = pixels
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8, 255])
            .collect();
        let img = image::RgbaImage::from_raw(w, h, buffer).expect("pixel buffer size mismatch");
        img.save(&out_path).expect("could not write PNG");
        println!("Window -> {out_path} ({w}x{h})");
    } else if png_mode {
        let out_path = args.next().unwrap_or_else(|| "output.png".into());
        // A trailing argument is a find-in-page query, so highlighting can be
        // eyeballed headlessly.
        render_to_png(&html, &css, &out_path, &address, args.next());
    } else if restore_session {
        // No target given: pick up where the last session left off.
        if !app::run_window_restoring_session(engine) {
            let engine = fonts::build_engine();
            app::run_window(engine, html, css, address);
        }
    } else {
        app::run_window(engine, html, css, address);
    }
}

/// Render a page to a PNG — in a **renderer process**, not this one.
///
/// This is the path where a page is opened, drawn once and finished with, so
/// nothing has to survive the exchange and the content never touches the
/// process holding the profile. The window still renders in-process; that
/// protocol is the next increment (see `renderer`'s note).
fn render_to_png(html: &str, css: &str, out_path: &str, base: &str, find: Option<String>) {
    let loader = ShellLoader::new(base.to_string());
    let Some(frame) = renderer::render_in_child(
        html,
        css,
        800.0,
        600.0,
        find.as_deref(),
        &loader,
    ) else {
        eprintln!("the renderer process did not answer");
        return;
    };
    let img =
        image::RgbaImage::from_raw(frame.width as u32, frame.height as u32, frame.pixels)
            .expect("pixel buffer size mismatch");
    img.save(out_path).expect("could not write PNG");
    println!("Rendered -> {out_path} ({}x{})", frame.width, frame.height);
}
