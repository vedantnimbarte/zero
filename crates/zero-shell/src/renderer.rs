//! The renderer process: where page content is parsed, run and drawn.
//!
//! A page's HTML, CSS and JavaScript are the most hostile input a browser
//! handles, and until now they were handled in the process that also holds your
//! history, your cookies and the key that protects them. This moves that work
//! into a child process that has given up everything it can before it reads a
//! byte: no privileges on its token, and the same startup mitigations the
//! browser applies to itself.
//!
//! The child has **no network of its own**. Every subresource it wants — a
//! stylesheet, an image — travels back up the pipe for the parent to fetch,
//! which is what keeps cookies and the tracker list on the trusted side.
//!
//! ponytail: this is used where the exchange is one round trip and no state
//! outlives it — the headless render paths, which is exactly where a page is
//! opened and drawn once. Interactive tabs still render in-process, because
//! every click, keystroke and hover would have to cross the same pipe and that
//! protocol is the next increment, not this one. `docs/03-ROADMAP.md` says so
//! plainly rather than leaving the gap implied.

use crate::wire::{self, Msg};
use std::process::{Child, Command, Stdio};
use zero_engine::ResourceLoader;

/// Run as the renderer: read one request, answer it, exit.
///
/// Called from `main` when the process was started with `--render-worker`; the
/// browser never calls it in-process.
pub fn serve() {
    crate::sandbox::harden();
    crate::sandbox::drop_privileges();

    let mut input = std::io::stdin();
    let Ok(Some(request)) = wire::read(&mut input) else { return };
    if request.name != "render" {
        return;
    }
    let html = request.str_at(0).to_string();
    let css = request.str_at(1).to_string();
    let find = request.str_at(2).to_string();
    let (width, height) = (request.num_at(0) as f32, request.num_at(1) as f32);

    // Fonts are read from disk before anything else, so a future jail can close
    // the filesystem behind us.
    let engine = crate::fonts::build_engine();
    let loader = PipeLoader;
    let mut doc = zero_engine::Document::load_with(&html, &css, std::rc::Rc::new(PipeLoader));
    if !find.is_empty() {
        doc.set_find(Some(find));
    }
    let page = engine.render_document(&mut doc, width, height, &loader);

    // Pixels as RGBA bytes, then the parts of the page the parent needs to know
    // about: its title, and how big the picture is.
    let mut pixels = Vec::with_capacity(page.canvas.pixels.len() * 4);
    for p in &page.canvas.pixels {
        pixels.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }
    let answer = Msg::new("frame")
        .text(doc.title())
        .num(page.canvas.width as f64)
        .num(page.canvas.height as f64)
        .blob(pixels);
    let mut output = std::io::stdout();
    let _ = wire::write(&mut output, &answer);
    for line in page.console {
        eprintln!("[js] {line}");
    }
}

/// The child's loader: it owns no sockets, so every fetch is a question for the
/// parent, asked on stdout and answered on stdin.
struct PipeLoader;

impl ResourceLoader for PipeLoader {
    fn load(&self, url: &str) -> Option<Vec<u8>> {
        self.load_all(&[url.to_string()]).into_iter().next().flatten()
    }

    fn load_all(&self, urls: &[String]) -> Vec<Option<Vec<u8>>> {
        if urls.is_empty() {
            return Vec::new();
        }
        let mut request = Msg::new("fetch");
        for url in urls {
            request = request.text(url.clone());
        }
        if wire::write(&mut std::io::stdout(), &request).is_err() {
            return vec![None; urls.len()];
        }
        // The parent answers with one blob per URL, each length-prefixed inside
        // the blob so an empty answer and a missing one stay distinguishable.
        let Ok(Some(answer)) = wire::read(&mut std::io::stdin()) else {
            return vec![None; urls.len()];
        };
        split_blobs(&answer.blob, urls.len())
    }
}

/// A rendered page as the parent receives it.
pub struct Frame {
    /// The page's own title, which is what a tab would be labelled with.
    #[allow(dead_code)] // read once the window renders through this too
    pub title: String,
    pub width: usize,
    pub height: usize,
    /// RGBA, row-major.
    pub pixels: Vec<u8>,
}

/// Render `html` in a child process, fetching whatever it asks for through
/// `loader` — which stays here, in the process allowed to have it.
pub fn render_in_child(
    html: &str,
    css: &str,
    width: f32,
    height: f32,
    find: Option<&str>,
    loader: &dyn ResourceLoader,
) -> Option<Frame> {
    let exe = std::env::current_exe().ok()?;
    let mut child: Child = Command::new(exe)
        .arg("--render-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .ok()?;
    let mut to_child = child.stdin.take()?;
    let mut from_child = child.stdout.take()?;

    let request = Msg::new("render")
        .text(html)
        .text(css)
        .text(find.unwrap_or(""))
        .num(width as f64)
        .num(height as f64);
    wire::write(&mut to_child, &request).ok()?;

    // Serve the child's fetches until it sends the frame it was asked for.
    loop {
        let message = match wire::read(&mut from_child) {
            Ok(Some(message)) => message,
            _ => break,
        };
        match message.name.as_str() {
            "fetch" => {
                let answers = loader.load_all(&message.text);
                let mut blob = Vec::new();
                for answer in answers {
                    match answer {
                        // A present-but-empty body and a failed fetch are not
                        // the same thing, so the length carries a flag.
                        Some(bytes) => {
                            blob.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                            blob.extend_from_slice(&bytes);
                        }
                        None => blob.extend_from_slice(&u32::MAX.to_le_bytes()),
                    }
                }
                if wire::write(&mut to_child, &Msg::new("fetched").blob(blob)).is_err() {
                    break;
                }
            }
            "frame" => {
                let _ = child.wait();
                return Some(Frame {
                    title: message.str_at(0).to_string(),
                    width: message.num_at(0) as usize,
                    height: message.num_at(1) as usize,
                    pixels: message.blob,
                });
            }
            _ => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    None
}

/// Undo [`render_in_child`]'s packing: `u32::MAX` marks a fetch that failed.
fn split_blobs(mut rest: &[u8], expected: usize) -> Vec<Option<Vec<u8>>> {
    let mut out = Vec::with_capacity(expected);
    while out.len() < expected && rest.len() >= 4 {
        let (header, body) = rest.split_at(4);
        let len = u32::from_le_bytes(header.try_into().unwrap_or([0; 4]));
        if len == u32::MAX {
            out.push(None);
            rest = body;
            continue;
        }
        let len = (len as usize).min(body.len());
        let (bytes, after) = body.split_at(len);
        out.push(Some(bytes.to_vec()));
        rest = after;
    }
    out.resize_with(expected, || None);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_answers_keep_empty_and_missing_apart() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&0u32.to_le_bytes()); // present, empty
        blob.extend_from_slice(&u32::MAX.to_le_bytes()); // failed
        blob.extend_from_slice(&2u32.to_le_bytes());
        blob.extend_from_slice(b"hi");

        let answers = split_blobs(&blob, 3);
        assert_eq!(answers[0], Some(Vec::new()));
        assert_eq!(answers[1], None);
        assert_eq!(answers[2], Some(b"hi".to_vec()));
        // A truncated stream yields misses, never a panic.
        assert_eq!(split_blobs(&[1, 2], 2), vec![None, None]);
    }
}
