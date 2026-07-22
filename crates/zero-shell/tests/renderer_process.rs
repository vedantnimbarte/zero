//! The renderer process, end to end: the browser binary spawning itself.
//!
//! A unit test cannot cover a process boundary — the whole point is that the
//! work happens somewhere else — so this drives the real executable the way a
//! person would, and checks that what comes back is a picture of the page.

use std::process::Command;

/// Draw `html` through the `--png` path, which renders in a child process, and
/// return the PNG's bytes.
fn render(name: &str, html: &str) -> Vec<u8> {
    let dir = std::env::temp_dir().join("zero-renderer-test");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let page = dir.join(format!("{name}.html"));
    let out = dir.join(format!("{name}.png"));
    std::fs::write(&page, html).expect("write page");
    let _ = std::fs::remove_file(&out);

    let status = Command::new(env!("CARGO_BIN_EXE_zero"))
        .args(["--png", &page.to_string_lossy(), &out.to_string_lossy()])
        .status()
        .expect("run the browser");
    assert!(status.success(), "the browser exited with {status}");
    std::fs::read(&out).expect("the renderer produced no PNG")
}

#[test]
fn a_page_is_drawn_by_a_process_that_is_not_this_one() {
    let png = render(
        "shapes",
        "<html><body style=\"x\"><div id=\"a\"></div>\
         <style>#a { width: 40px; height: 40px; background: #ff0000; }</style>\
         </body></html>",
    );
    // A PNG, of the size the headless path draws at.
    assert_eq!(&png[..4], b"\x89PNG", "not a PNG");
    let width = u32::from_be_bytes(png[16..20].try_into().expect("IHDR width"));
    let height = u32::from_be_bytes(png[20..24].try_into().expect("IHDR height"));
    assert_eq!((width, height), (800, 600));
}

#[test]
fn a_page_that_kills_the_renderer_does_not_take_the_browser_with_it() {
    // Deeply nested markup is the cheap way to lean on the parser from a page:
    // a few hundred bytes of `<div>` used to run the stack out. It is capped
    // now, so this renders — and if a future change breaks that, the parent
    // still has to come back and say so rather than dying alongside its
    // renderer, which is the point of the split.
    let hostile = format!("<html><body>{}</body></html>", "<div>".repeat(5_000));
    let status = Command::new(env!("CARGO_BIN_EXE_zero"))
        .args([
            "--png",
            &{
                let dir = std::env::temp_dir().join("zero-renderer-test");
                std::fs::create_dir_all(&dir).expect("scratch dir");
                let path = dir.join("hostile.html");
                std::fs::write(&path, hostile).expect("write page");
                path.to_string_lossy().into_owned()
            },
            &std::env::temp_dir()
                .join("zero-renderer-test")
                .join("hostile.png")
                .to_string_lossy(),
        ])
        .status()
        .expect("run the browser");
    assert!(status.success(), "the browser must survive its renderer");
}
