//! The renderer process, end to end: the browser binary spawning itself.
//!
//! A unit test cannot cover a process boundary — the whole point is that the
//! work happens somewhere else — so this drives the real executable the way a
//! person would, and checks that what comes back is a picture of the page.

use std::io::{Read, Write};
use std::process::{Command, Stdio};

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

#[test]
fn one_render_worker_answers_two_requests_before_it_exits() {
    // `zero-shell`'s `wire` module is a private implementation detail of the
    // binary crate, unreachable from an integration test — so this speaks
    // just enough of its length-prefixed framing by hand to drive the worker
    // directly, the way `app.rs` will once interactive tabs use this pipe
    // for more than one frame. That's the thing this test exists to prove:
    // one process, two `render` requests, two `frame` replies, then a clean
    // exit once its input is closed — not the one-shot-and-exit it used to be.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zero"))
        .arg("--render-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the renderer worker");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut store = std::collections::HashMap::new();

    for color in ["#ff0000", "#0000ff"] {
        let html = format!(
            "<div id=a></div><style>#a {{ width: 4px; height: 4px; background: {color}; }}</style>"
        );
        write_msg(&mut stdin, "render", &[&html, "", ""], &[10.0, 10.0]);
        let frame =
            read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame in reply to each request");
        assert_eq!((frame.width, frame.height), (10, 10));
        // The pixel at (0,0) is the div's own colour — proof this reply
        // reflects *this* request's HTML, not a stale one from before.
        let want = if color == "#ff0000" { [0xff, 0, 0, 0xff] } else { [0, 0, 0xff, 0xff] };
        assert_eq!(&frame.pixels[0..4], &want, "wrong colour for {color}");
    }

    // Nothing left to ask: closing stdin is how a caller says so, and the
    // worker is expected to notice the resulting EOF and exit on its own —
    // not hang waiting for a request that is never coming.
    drop(stdin);
    let status = child.wait().expect("wait for the worker to exit");
    assert!(status.success(), "the worker should exit cleanly on EOF, got {status}");
}

#[test]
fn focus_and_typing_mutate_the_same_persistent_document_across_messages() {
    // The point of a long-lived renderer is that `click`/`focus`/`insert_text`
    // act on *one* `Document` that survives between them — unlike `render`,
    // which starts a fresh one. This drives exactly the sequence `app.rs`
    // will: read node ids back out of the first frame's element rects (real
    // hit-testing data, not a hardcoded id), focus a field by id, type into
    // it, then backspace — and checks the *rendered pixels* changed and then
    // changed back, since that's the only way to know from outside the
    // process that the typed character actually reached the field.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zero"))
        .arg("--render-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the renderer worker");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut store = std::collections::HashMap::new();

    let html = "<input id=box>\
                 <style>#box { width: 100px; height: 24px; background: #ffffff; \
                         color: #000000; font-size: 18px; }</style>";
    write_msg(&mut stdin, "render", &[html, "", ""], &[200.0, 60.0]);
    let initial = read_frame(&mut stdout, &mut stdin, &mut store).expect("the first frame");
    assert!(!initial.is_focused, "nothing has been focused yet");
    let (node_id, field) =
        initial.rect_by_id("box").expect("the input's own rect, for real hit-testing");

    // A patch of the field just past its left padding, where a typed
    // character's ink would land — empty (background colour) before typing.
    let probe = (field.0 as usize + 8, field.1 as usize + field.3 as usize / 2);
    assert!(initial.region_is_blank(probe, 8, 8), "an empty field has no ink to find");

    // `click` collapses the precedence `app.rs` used to apply itself across
    // two messages (focus wins over a click handler) into one: the renderer
    // tries `focus` first and only falls back to `click` when that refuses,
    // so clicking a text field should focus it in a single round trip.
    write_msg(&mut stdin, "click", &[], &[node_id as f64]);
    let clicked = read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame after click");
    assert!(clicked.is_focused, "clicking a text field should focus it, not just click it");

    write_msg(&mut stdin, "blur", &[], &[]);
    read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame after blur");

    write_msg(&mut stdin, "focus", &[], &[node_id as f64]);
    let focused = read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame after focus");
    assert!(focused.is_focused, "focus should reach the same document the rect came from");

    write_msg(&mut stdin, "insert_text", &["MMMM"], &[]);
    let typed = read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame after typing");
    assert!(!typed.region_is_blank(probe, 8, 8), "typed text should now paint something");

    // Every message gets its own `frame` reply — four backspaces means four
    // replies queued up, and only the last one has erased everything typed.
    for _ in 0.."MMMM".len() {
        write_msg(&mut stdin, "backspace", &[], &[]);
        read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame after each backspace");
    }
    // A focused field always shows a `|` caret (`lib.rs`'s `sync_form_fields`),
    // so "empty" and "blank" aren't the same thing while it still has focus —
    // blur it first, which is its own real message to prove works.
    write_msg(&mut stdin, "blur", &[], &[]);
    let erased = read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame after blur");
    assert!(!erased.is_focused, "blur should reach the same document focus did");
    assert!(
        erased.region_is_blank(probe, 8, 8),
        "backspace should have acted on the same field insert_text did, not a fresh one"
    );

    drop(stdin);
    let status = child.wait().expect("wait for the worker to exit");
    assert!(status.success(), "the worker should exit cleanly on EOF, got {status}");
}

#[test]
fn localstorage_round_trips_through_the_pipe_to_a_real_kv_store() {
    // `PipeStore` (the child's `localStorage`) asks its parent for every read
    // and fires every write at it — this proves that channel actually
    // carries values both ways, by running real JS that sets a key, reads it
    // straight back, and only then paints a marker green. If the write or
    // the read silently no-op'd (a `store: None` headless render, say, or a
    // protocol mismatch), the marker stays white and the test catches it.
    let mut child = Command::new(env!("CARGO_BIN_EXE_zero"))
        .arg("--render-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn the renderer worker");
    let mut stdin = child.stdin.take().expect("child stdin");
    let mut stdout = child.stdout.take().expect("child stdout");
    let mut store = std::collections::HashMap::new();

    let html = "<div id=marker></div>\
                 <style>#marker { width: 20px; height: 20px; background: #ffffff; } \
                         #marker.ok { background: #00ff00; }</style>\
                 <script>\
                   localStorage.setItem('k', 'hello');\
                   var v = localStorage.getItem('k');\
                   if (v == 'hello') { document.getElementById('marker').className = 'ok'; }\
                 </script>";
    write_msg(&mut stdin, "render", &[html, "", ""], &[40.0, 40.0]);
    let frame = read_frame(&mut stdout, &mut stdin, &mut store).expect("a frame");

    assert_eq!(store.get("k").map(String::as_str), Some("hello"), "the set should have landed");
    let (_, marker) = frame.rect_by_id("marker").expect("the marker's own rect");
    let (x, y) = (marker.0 as usize + 5, marker.1 as usize + 5);
    let i = (y * frame.width + x) * 4;
    assert_eq!(
        &frame.pixels[i..i + 3],
        &[0, 0xff, 0],
        "the read should have returned what was just written, turning the marker green"
    );

    drop(stdin);
    let status = child.wait().expect("wait for the worker to exit");
    assert!(status.success(), "the worker should exit cleanly on EOF, got {status}");
}

struct TestFrame {
    width: usize,
    height: usize,
    is_focused: bool,
    pixels: Vec<u8>,
    /// `(node_id, id_attribute, x, y, width, height)`.
    element_rects: Vec<(usize, String, f32, f32, f32, f32)>,
}

impl TestFrame {
    /// The node id and rect of the element with this `id` attribute — how a
    /// real caller turns "the box the user clicked" into the node id a
    /// `click`/`focus` message needs, using data the frame already reported.
    fn rect_by_id(&self, id: &str) -> Option<(usize, (f32, f32, f32, f32))> {
        self.element_rects
            .iter()
            .find(|(_, rect_id, ..)| rect_id == id)
            .map(|(node_id, _, x, y, w, h)| (*node_id, (*x, *y, *w, *h)))
    }

    /// Whether every pixel in an `w`x`h` box at `(x, y)` is pure white — the
    /// field's own background, so "not blank" means something painted there.
    fn region_is_blank(&self, (x, y): (usize, usize), w: usize, h: usize) -> bool {
        (y..y + h).all(|py| {
            (x..x + w).all(|px| {
                if px >= self.width || py >= self.height {
                    return true;
                }
                let i = (py * self.width + px) * 4;
                self.pixels[i..i + 3] == [0xff, 0xff, 0xff]
            })
        })
    }
}

fn write_msg(w: &mut impl Write, name: &str, text: &[&str], nums: &[f64]) {
    let mut body = Vec::new();
    put_str(&mut body, name);
    put_u32(&mut body, text.len() as u32);
    for s in text {
        put_str(&mut body, s);
    }
    put_u32(&mut body, nums.len() as u32);
    for n in nums {
        body.extend_from_slice(&n.to_le_bytes());
    }
    put_u32(&mut body, 0); // blob: no message this test sends carries one
    let mut framed = (body.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&body);
    w.write_all(&framed).expect("write the message");
    w.flush().expect("flush the message");
}

/// One decoded message, before it's known to be a `frame` or something this
/// test has to answer itself.
struct RawMsg {
    name: String,
    text: Vec<String>,
    nums: Vec<f64>,
    blob: Vec<u8>,
}

fn read_msg(r: &mut impl Read) -> Option<RawMsg> {
    let len = get_u32(r)?;
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).ok()?;
    let mut cursor = body.as_slice();

    let name = get_str(&mut cursor)?;
    let text_count = get_u32(&mut cursor)?;
    let mut text = Vec::with_capacity(text_count as usize);
    for _ in 0..text_count {
        text.push(get_str(&mut cursor)?);
    }
    let num_count = get_u32(&mut cursor)?;
    let mut nums = Vec::with_capacity(num_count as usize);
    for _ in 0..num_count {
        let mut buf = [0u8; 8];
        cursor.read_exact(&mut buf).ok()?;
        nums.push(f64::from_le_bytes(buf));
    }
    let blob_len = get_u32(&mut cursor)?;
    let mut blob = vec![0u8; blob_len as usize];
    cursor.read_exact(&mut blob).ok()?;

    Some(RawMsg { name, text, nums, blob })
}

/// Decode a `frame` message's body, per the schema `write_frame` in
/// `renderer.rs` documents: `nums` is `[w, h, uses_hover, animating,
/// is_focused, rect_count, link_count, match_count]` then `rect_count`
/// groups of `[node_id, x, y, w, h]`, then link and find-match rects; `text`
/// is `[title, ...rect ids, ...link hrefs]` in the same order.
fn decode_frame(msg: RawMsg) -> TestFrame {
    let get = |i: usize| *msg.nums.get(i).unwrap_or(&0.0);
    let (width, height) = (get(0) as usize, get(1) as usize);
    let is_focused = get(4) != 0.0;
    let rect_count = get(5) as usize;
    let mut at = 8; // past the fixed [w,h,uses_hover,animating,is_focused,rects,links,matches]
    let mut element_rects = Vec::with_capacity(rect_count);
    for i in 0..rect_count {
        let (node_id, x, y, w, h) =
            (get(at) as usize, get(at + 1) as f32, get(at + 2) as f32, get(at + 3) as f32, get(at + 4) as f32);
        at += 5;
        let id = msg.text.get(1 + i).cloned().unwrap_or_default();
        element_rects.push((node_id, id, x, y, w, h));
    }

    TestFrame { width, height, is_focused, pixels: msg.blob, element_rects }
}

/// Read messages until a `frame` arrives, answering `storage_get`/`_set`/
/// `_remove`/`_clear` against `store` along the way — the same role
/// `renderer::round_trip`'s loop plays for the real browser process, stood
/// up by hand here since that function is private to the binary crate.
/// `fetch` answers "missing" for everything: nothing in these tests loads a
/// subresource.
fn read_frame(
    r: &mut impl Read,
    w: &mut impl Write,
    store: &mut std::collections::HashMap<String, String>,
) -> Option<TestFrame> {
    loop {
        let msg = read_msg(r)?;
        match msg.name.as_str() {
            "frame" => return Some(decode_frame(msg)),
            "storage_get" => match store.get(&msg.text[0]) {
                Some(value) => write_msg(w, "storage_value", &[value], &[1.0]),
                None => write_msg(w, "storage_value", &[""], &[0.0]),
            },
            "storage_set" => {
                store.insert(msg.text[0].clone(), msg.text[1].clone());
            }
            "storage_remove" => {
                store.remove(&msg.text[0]);
            }
            "storage_clear" => store.clear(),
            "fetch" => {
                let mut body = Vec::new();
                for _ in &msg.text {
                    body.extend_from_slice(&u32::MAX.to_le_bytes());
                }
                let mut framed = Vec::new();
                put_str(&mut framed, "fetched");
                put_u32(&mut framed, 0);
                put_u32(&mut framed, 0);
                put_u32(&mut framed, body.len() as u32);
                framed.extend_from_slice(&body);
                let mut out = (framed.len() as u32).to_le_bytes().to_vec();
                out.extend_from_slice(&framed);
                w.write_all(&out).ok()?;
                w.flush().ok()?;
            }
            _ => return None,
        }
    }
}

fn put_u32(out: &mut Vec<u8>, n: u32) {
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_u32(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn get_u32(r: &mut impl Read) -> Option<u32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf).ok()?;
    Some(u32::from_le_bytes(buf))
}

fn get_str(r: &mut impl Read) -> Option<String> {
    let len = get_u32(r)?;
    let mut buf = vec![0u8; len as usize];
    r.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}
