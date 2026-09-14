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
//! `app.rs` spawns one of these per tab via [`TabRenderer`] and sends it
//! input (`click`, `insert_text`, `hover`, ...) instead of holding a
//! `Document` itself — interactive tabs, not just the headless render paths,
//! now run page content out of process. `docs/03-ROADMAP.md`'s note on this
//! being "the next increment" is what this turned into.
//!
//! ponytail: no reader thread yet — every message on [`TabRenderer`] blocks
//! the UI thread until its reply arrives, so a page that hangs mid-script
//! freezes the window rather than just that tab. A background reader plus a
//! crash-and-respawn story is the next increment; until then a `None` reply
//! anywhere in `app.rs` just leaves a tab showing its last good frame.

use crate::wire::{self, Msg};
use std::io::Write;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use zero_engine::{Engine, ResourceLoader};

/// Run as the renderer: answer requests until the parent closes its end of
/// the pipe, then exit.
///
/// Called from `main` when the process was started with `--render-worker`; the
/// browser never calls it in-process. A clean EOF — not a "goodbye" message —
/// is what ends this loop: the parent has nothing left to ask, so there is
/// never a reason to keep the process around waiting.
pub fn serve() {
    crate::sandbox::harden();
    crate::sandbox::drop_privileges();

    // Fonts are read from disk before anything else, so a future jail can
    // close the filesystem behind us — and only once, since every request
    // after the first reuses the engine already built here.
    let engine = crate::fonts::build_engine();
    let mut input = std::io::stdin();
    let mut output = std::io::stdout();

    // The document a `render` request creates lives here, across whatever
    // `click`/`focus`/`insert_text`/... messages follow it — that persistence
    // is the entire point: those methods mutate the *same* `Document`, the
    // way a page's own state survives between a person's keystrokes.
    let mut session: Option<Session> = None;

    while let Ok(Some(request)) = wire::read(&mut input) {
        match request.name.as_str() {
            "render" => session = Some(Session::load(&request)),
            // A pure read of already-loaded state — no script runs and
            // nothing changes, so it gets its own reply instead of a `frame`
            // (which would mean re-rendering just to answer a text query).
            "page_text" => {
                if let Some(session) = session.as_ref() {
                    write_page_text(session, &mut output);
                }
                continue;
            }
            "click" | "focus" | "blur" | "insert_text" | "backspace" | "resize" | "find"
            | "submit" | "hover" => {
                match session.as_mut() {
                    Some(session) => session.apply(&request),
                    // An interaction with nothing loaded yet is a protocol
                    // error, not a page doing something unusual.
                    None => break,
                }
            }
            _ => break,
        }
        let Some(session) = session.as_mut() else { continue };
        write_frame(&engine, session, &mut output);
    }
}

/// Answer `page_text` with the document's own text and heading outline —
/// what the AI assistant panel reads, packed as `text: [page_text,
/// ...heading text]`, `nums: [...heading level]` (one level per heading, in
/// the same order).
fn write_page_text(session: &Session, output: &mut std::io::Stdout) {
    let mut answer = Msg::new("text_content").text(session.doc.page_text());
    let headings = session.doc.headings();
    for (_, text) in &headings {
        answer = answer.text(text.clone());
    }
    for (level, _) in &headings {
        answer = answer.num(*level as f64);
    }
    let _ = wire::write(output, &answer);
}

/// A loaded document plus the viewport it was last asked to draw at — the
/// state one renderer process holds for the tab's whole life, not just one
/// request.
struct Session {
    doc: zero_engine::Document,
    width: f32,
    height: f32,
    /// Set by `submit`, read and cleared by the `frame` reply that follows it
    /// — reported once, not on every later frame this session ever sends.
    pending_submission: Option<zero_engine::Submission>,
    /// Whether the last `click` took focus or ran a script handler — same
    /// lifetime as `pending_submission`. `app.rs` needs this to know whether
    /// to still check for a link at the click point: a click's own precedence
    /// (focus, then a handler) beats a link underneath it, exactly as it did
    /// when `app.rs` made that decision itself from `doc.click()`'s return.
    click_handled: bool,
    /// This process's own clock, so CSS transitions have one to animate
    /// against without `app.rs` having to thread a timestamp through every
    /// message — a renderer that outlives one request can just watch its own
    /// wall clock instead.
    created: std::time::Instant,
}

impl Session {
    /// Handle a `render` request: parse a fresh document. This is a real
    /// navigation (or the tab's first page), so nothing about the old one
    /// — focus, typed text, script state — should or does survive it.
    fn load(request: &Msg) -> Session {
        let html = request.str_at(0);
        let css = request.str_at(1);
        let find = request.str_at(2);
        // Always wired to a `PipeStore`: this process never knows whether its
        // parent is a real tab (backing it with a per-site file) or a
        // one-shot headless render with nothing to persist to — that
        // decision belongs entirely to whoever answers `storage_get`.
        let mut doc = zero_engine::Document::load_hosted(
            html,
            css,
            Some(std::rc::Rc::new(PipeLoader)),
            Some(std::rc::Rc::new(PipeStore)),
        );
        if !find.is_empty() {
            doc.set_find(Some(find.to_string()));
        }
        Session {
            doc,
            width: request.num_at(0) as f32,
            height: request.num_at(1) as f32,
            pending_submission: None,
            click_handled: false,
            created: std::time::Instant::now(),
        }
    }

    /// Mutate the live document in response to one input message. Every case
    /// here is a direct call into the same [`zero_engine::Document`] `app.rs`
    /// calls today — only which process is making the call changes.
    fn apply(&mut self, request: &Msg) {
        // Meaningful only on the reply to the `click` that just set it —
        // every other message clears it, the same way `pending_submission`
        // is only ever read once.
        self.click_handled = false;
        match request.name.as_str() {
            "click" => {
                let node_id = request.num_at(0) as usize;
                // Blur unconditionally first — clicking anywhere, hit or not,
                // commits whatever field was focused before (firing `change`
                // if its value actually moved), exactly as `app.rs` did by
                // calling `blur()` ahead of every click. Only then does a
                // field take focus instead of running its click handler; that
                // precedence collapses into one round trip now that both
                // calls reach the same process.
                self.doc.blur();
                self.click_handled =
                    self.doc.focus(node_id) || self.doc.click(node_id);
            }
            "focus" => {
                self.doc.focus(request.num_at(0) as usize);
            }
            "blur" => self.doc.blur(),
            "insert_text" => {
                self.doc.insert_text(request.str_at(0));
            }
            "backspace" => {
                self.doc.backspace();
            }
            "resize" => {
                self.width = request.num_at(0) as f32;
                self.height = request.num_at(1) as f32;
            }
            "find" => {
                let query = request.str_at(0);
                self.doc.set_find((!query.is_empty()).then(|| query.to_string()));
            }
            "submit" => {
                self.pending_submission =
                    self.doc.focused_node().and_then(|id| self.doc.submit(id));
                self.doc.blur();
            }
            "hover" => {
                let id = request.num_at(0);
                self.doc.set_hover((id >= 0.0).then_some(id as usize));
            }
            _ => unreachable!("serve's match already filtered to these names"),
        }
    }
}

/// Draw the session's current document and write back everything the browser
/// needs to keep chrome-side hit-testing and compositing working without a
/// round trip per mouse move: not just pixels, but the element/link/find
/// boxes a headless render never had to report.
///
/// Packed into `Msg`'s plain `text`/`nums` arrays rather than a new format —
/// `nums` lays out as `[w, h, uses_hover, animating, is_focused, rect_count,
/// link_count, match_count]` followed by `rect_count` groups of
/// `[node_id, x, y, w, h]`, then `link_count` groups of `[x, y, w, h]`, then
/// `match_count` groups of `[x, y, w, h]`, then two trailing flags,
/// `has_submission` and `click_handled`; `text` is `[title, ...rect ids,
/// ...link hrefs, submit_action, submit_query]` in the same order — the
/// submission fields are always last, so decoding them needs no new offset
/// math, just `nums`/`text`'s own lengths. One schema, documented once, same
/// as every other message this pipe carries.
fn write_frame(engine: &Engine, session: &mut Session, output: &mut std::io::Stdout) {
    session.doc.set_time(session.created.elapsed().as_secs_f32() * 1000.0);
    let loader = PipeLoader;
    let page = engine.render_document(&mut session.doc, session.width, session.height, &loader);

    let mut pixels = Vec::with_capacity(page.canvas.pixels.len() * 4);
    for p in &page.canvas.pixels {
        pixels.extend_from_slice(&[p.r, p.g, p.b, p.a]);
    }

    let mut answer = Msg::new("frame").text(session.doc.title());
    answer = answer
        .num(page.canvas.width as f64)
        .num(page.canvas.height as f64)
        .num(page.uses_hover as u8 as f64)
        .num(page.animating as u8 as f64)
        .num(session.doc.is_focused() as u8 as f64)
        .num(page.element_rects.len() as f64)
        .num(page.links.len() as f64)
        .num(page.find_matches.len() as f64);
    for r in &page.element_rects {
        answer = answer.num(r.node_id as f64).num(r.x as f64).num(r.y as f64);
        answer = answer.num(r.width as f64).num(r.height as f64);
    }
    for l in &page.links {
        answer = answer.num(l.x as f64).num(l.y as f64).num(l.width as f64).num(l.height as f64);
    }
    for m in &page.find_matches {
        answer = answer.num(m.x as f64).num(m.y as f64).num(m.width as f64).num(m.height as f64);
    }
    for r in &page.element_rects {
        answer = answer.text(r.id.clone());
    }
    for l in &page.links {
        answer = answer.text(l.href.clone());
    }
    let submission = session.pending_submission.take();
    answer = answer
        .num(submission.is_some() as u8 as f64)
        .num(session.click_handled as u8 as f64)
        .text(submission.as_ref().map_or("", |s| &s.action))
        .text(submission.as_ref().map_or("", |s| &s.query));
    answer = answer.blob(pixels);

    let _ = wire::write(output, &answer);
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

/// The child's `localStorage`: like [`PipeLoader`], it owns nothing itself —
/// every read is a blocking question for the parent, and every write is
/// fire-and-forget, exactly as a script calling `localStorage.setItem`
/// expects never to wait on.
struct PipeStore;

impl zero_engine::KeyValueStore for PipeStore {
    fn get(&self, key: &str) -> Option<String> {
        let request = Msg::new("storage_get").text(key);
        if wire::write(&mut std::io::stdout(), &request).is_err() {
            return None;
        }
        let Ok(Some(answer)) = wire::read(&mut std::io::stdin()) else { return None };
        (answer.num_at(0) != 0.0).then(|| answer.str_at(0).to_string())
    }

    fn set(&self, key: &str, value: &str) {
        let _ = wire::write(
            &mut std::io::stdout(),
            &Msg::new("storage_set").text(key).text(value),
        );
    }

    fn remove(&self, key: &str) {
        let _ = wire::write(&mut std::io::stdout(), &Msg::new("storage_remove").text(key));
    }

    fn clear(&self) {
        let _ = wire::write(&mut std::io::stdout(), &Msg::new("storage_clear"));
    }
}

/// A rendered page as the parent receives it: pixels, plus everything chrome
/// needs to hit-test clicks and mouse moves against without asking the
/// renderer per pixel. The headless one-shot path only ever reads the first
/// four fields; an interactive tab reads the rest.
pub struct Frame {
    /// The page's own title, which is what a tab would be labelled with.
    pub title: String,
    pub width: usize,
    pub height: usize,
    /// RGBA, row-major.
    pub pixels: Vec<u8>,
    pub uses_hover: bool,
    pub animating: bool,
    pub is_focused: bool,
    pub element_rects: Vec<zero_engine::ElementRect>,
    pub links: Vec<zero_engine::LinkArea>,
    pub find_matches: Vec<zero_engine::layout::Rect>,
    /// Set only on the reply to a `submit` message, and only when the
    /// focused field was actually inside a `<form>`.
    pub submission: Option<zero_engine::Submission>,
    /// Set only on the reply to a `click` message: whether it took focus or
    /// ran a script handler. `false` means the click point still needs
    /// checking against `links`, exactly as `app.rs` used to decide from
    /// `doc.click()`'s own return value.
    pub click_handled: bool,
}

/// Undo `write_frame`'s packing (see that function for the schema).
fn decode_frame(msg: Msg) -> Frame {
    let get = |i: usize| msg.num_at(i);
    let (width, height) = (get(0) as usize, get(1) as usize);
    let (rect_count, link_count, match_count) = (get(5) as usize, get(6) as usize, get(7) as usize);

    let mut at = 8;
    let mut element_rects = Vec::with_capacity(rect_count);
    for i in 0..rect_count {
        let rect = zero_engine::ElementRect {
            node_id: get(at) as usize,
            id: msg.str_at(1 + i).to_string(),
            x: get(at + 1) as f32,
            y: get(at + 2) as f32,
            width: get(at + 3) as f32,
            height: get(at + 4) as f32,
        };
        at += 5;
        element_rects.push(rect);
    }
    let mut links = Vec::with_capacity(link_count);
    for i in 0..link_count {
        let link = zero_engine::LinkArea {
            href: msg.str_at(1 + rect_count + i).to_string(),
            x: get(at) as f32,
            y: get(at + 1) as f32,
            width: get(at + 2) as f32,
            height: get(at + 3) as f32,
        };
        at += 4;
        links.push(link);
    }
    let mut find_matches = Vec::with_capacity(match_count);
    for _ in 0..match_count {
        find_matches.push(zero_engine::layout::Rect {
            x: get(at) as f32,
            y: get(at + 1) as f32,
            width: get(at + 2) as f32,
            height: get(at + 3) as f32,
        });
        at += 4;
    }

    let submission = (get(at) != 0.0).then(|| zero_engine::Submission {
        action: msg.str_at(1 + rect_count + link_count).to_string(),
        query: msg.str_at(2 + rect_count + link_count).to_string(),
    });
    let click_handled = get(at + 1) != 0.0;

    Frame {
        title: msg.str_at(0).to_string(),
        width,
        height,
        uses_hover: get(2) != 0.0,
        animating: get(3) != 0.0,
        is_focused: get(4) != 0.0,
        element_rects,
        links,
        find_matches,
        click_handled,
        submission,
        pixels: msg.blob,
    }
}

/// How one message from the child settles an exchange in progress: either
/// it's the `frame` this side was waiting for, a protocol error (treated the
/// same as the pipe having died — nothing to recover from mid-exchange), or
/// something this side has to answer or apply before continuing to wait.
enum Service {
    Frame(Frame),
    /// The reply to `page_text` — title/pixels aside, a pure text read.
    Text(String, Vec<(u8, String)>),
    Continue,
    Broken,
}

/// Answer (or apply) one non-`frame` message from the child — `fetch` via
/// `loader`, `storage_*` via `store` when there is one to answer with — or
/// decode it if it turns out to be the `frame` the caller is waiting for.
/// Shared by [`round_trip`] (reads the pipe directly) and [`TabRenderer`]'s
/// reply loop (reads from its background reader thread instead), since
/// what a message means is the same either way.
fn handle_service_message(
    message: Msg,
    to_child: &mut impl Write,
    loader: &dyn ResourceLoader,
    store: Option<&dyn zero_engine::KeyValueStore>,
) -> Service {
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
            match wire::write(to_child, &Msg::new("fetched").blob(blob)) {
                Ok(()) => Service::Continue,
                Err(_) => Service::Broken,
            }
        }
        // A headless render has no site to persist to (`store` is `None`);
        // every read answers "missing" and every write is silently dropped,
        // same as a script running with storage disabled would see.
        "storage_get" => {
            let answer = match store.and_then(|s| s.get(message.str_at(0))) {
                Some(value) => Msg::new("storage_value").num(1.0).text(value),
                None => Msg::new("storage_value").num(0.0).text(""),
            };
            match wire::write(to_child, &answer) {
                Ok(()) => Service::Continue,
                Err(_) => Service::Broken,
            }
        }
        "storage_set" => {
            if let Some(store) = store {
                store.set(message.str_at(0), message.str_at(1));
            }
            Service::Continue
        }
        "storage_remove" => {
            if let Some(store) = store {
                store.remove(message.str_at(0));
            }
            Service::Continue
        }
        "storage_clear" => {
            if let Some(store) = store {
                store.clear();
            }
            Service::Continue
        }
        "frame" => Service::Frame(decode_frame(message)),
        "text_content" => {
            let headings = message
                .nums
                .iter()
                .enumerate()
                .map(|(i, &level)| (level as u8, message.str_at(1 + i).to_string()))
                .collect();
            Service::Text(message.str_at(0).to_string(), headings)
        }
        _ => Service::Broken,
    }
}

/// Send one request and drive the exchange through to its `frame` reply —
/// answering whatever the child asks along the way rather than assuming a
/// request gets an immediate reply. Used only for a one-shot render: reads
/// the pipe on the calling thread directly, so a hung child hangs whoever
/// called this. [`TabRenderer`] does not use this — see its own reply loop.
fn round_trip(
    to_child: &mut ChildStdin,
    from_child: &mut ChildStdout,
    request: &Msg,
    loader: &dyn ResourceLoader,
    store: Option<&dyn zero_engine::KeyValueStore>,
) -> Option<Frame> {
    wire::write(to_child, request).ok()?;
    loop {
        let message = match wire::read(from_child) {
            Ok(Some(message)) => message,
            _ => return None,
        };
        match handle_service_message(message, to_child, loader, store) {
            Service::Frame(frame) => return Some(frame),
            Service::Continue => continue,
            // `round_trip` only ever sends `render`, which only ever gets a
            // `frame` back — a `text_content` here would be a different
            // exchange's reply arriving on the wrong one.
            Service::Text(..) | Service::Broken => return None,
        }
    }
}

/// The executable to spawn as a renderer worker.
///
/// `cargo test` recompiles this binary *as a test harness* — its own
/// `current_exe()` still points at a real file, but re-invoking that file
/// runs the generated test-runner `main`, not the CLI dispatch that
/// understands `--render-worker`, so a unit test that builds a [`Tab`] would
/// spawn a process that can never answer it. Cargo sets `CARGO_BIN_EXE_zero`
/// for exactly this situation — a compiled-in path to the real binary,
/// available to unit tests as well as integration tests — so prefer it
/// whenever it's present and fall back to `current_exe()` otherwise, which
/// covers every real run of the browser (nothing sets that variable outside
/// `cargo test`).
fn renderer_exe() -> Option<std::path::PathBuf> {
    option_env!("CARGO_BIN_EXE_zero")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
}

/// Render `html` in a child process, fetching whatever it asks for through
/// `loader` — which stays here, in the process allowed to have it. Spawns,
/// asks once, tears down: for a page opened and drawn once, not a tab kept
/// around between frames (see [`TabRenderer`] for that).
pub fn render_in_child(
    html: &str,
    css: &str,
    width: f32,
    height: f32,
    find: Option<&str>,
    loader: &dyn ResourceLoader,
) -> Option<Frame> {
    let exe = renderer_exe()?;
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
    // No `store`: a one-shot headless render has no site to persist to.
    let frame = round_trip(&mut to_child, &mut from_child, &request, loader, None);

    if frame.is_some() {
        // `serve` now answers requests in a loop rather than exiting after
        // one; a one-shot caller has to say "nothing more is coming" itself,
        // by closing its end of the pipe, or the child sits waiting for a
        // next request forever and this `wait()` never returns.
        drop(to_child);
        let _ = child.wait();
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }
    frame
}

/// How long a call waits for its reply before treating the renderer as hung.
///
/// Layout and paint of one page is milliseconds — but the very first `render`
/// a freshly spawned child answers pays for `fonts::build_engine()` first,
/// which reads every candidate system font off disk, several of them
/// multi-megabyte CJK `.ttc` files (`fonts.rs`'s own `ponytail:` note says as
/// much). Measured cold on real hardware, that one-time cost alone ran to
/// ~17 seconds — an 8-second ceiling was killing a renderer that was working
/// the whole time, not a hung one. Every message after that first one is
/// cheap (the engine is already built), so one generous ceiling covers both
/// without weakening what this exists to catch: a script that never returns.
const REPLY_TIMEOUT: Duration = Duration::from_secs(45);

/// A renderer process kept alive for one interactive tab's whole life,
/// carrying its own document between messages — unlike [`render_in_child`],
/// which tears its child down the moment it has an answer.
///
/// A dedicated background thread owns the read side of the pipe and forwards
/// every message it sees to [`TabRenderer`] over a channel; the call that is
/// waiting reads from that channel with [`REPLY_TIMEOUT`] instead of
/// blocking on the pipe directly. That bounds the one failure multi-process
/// is supposed to prevent: a page stuck in an infinite loop no longer
/// freezes the browser forever, only for up to `REPLY_TIMEOUT`, after which
/// this renderer reports itself dead (see [`TabRenderer::is_dead`]) rather
/// than hanging every future call the same way.
///
/// ponytail: calls still block the thread that makes them (the UI thread, in
/// `app.rs`) up to that ceiling — this fixes "frozen forever," not "not
/// instant." Turning every `TabRenderer` call into a fire-and-forget send
/// with the reply delivered later (a winit user event) is the next
/// increment, and a real rewrite of every `app.rs` call site when it lands.
pub struct TabRenderer {
    child: Child,
    stdin: ChildStdin,
    replies: mpsc::Receiver<Msg>,
    loader: std::rc::Rc<dyn ResourceLoader>,
    store: std::rc::Rc<dyn zero_engine::KeyValueStore>,
    /// Set once a call times out or the reader thread sees the pipe close.
    /// Every later call fails fast instead of waiting out the timeout again.
    dead: bool,
}

impl TabRenderer {
    /// Spawn a renderer and load a document into it. This is a real
    /// navigation: call it to replace a tab's previous renderer outright
    /// (dropping it kills the old process, see [`Drop`]) rather than trying
    /// to send a second page into one already running.
    pub fn spawn(
        html: &str,
        css: &str,
        width: f32,
        height: f32,
        loader: std::rc::Rc<dyn ResourceLoader>,
        store: std::rc::Rc<dyn zero_engine::KeyValueStore>,
    ) -> Option<(TabRenderer, Frame)> {
        let exe = renderer_exe()?;
        let mut child: Child = Command::new(exe)
            .arg("--render-worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let replies = spawn_reader(stdout);
        let mut renderer = TabRenderer { child, stdin, replies, loader, store, dead: false };
        let request = Msg::new("render")
            .text(html)
            .text(css)
            .text("")
            .num(width as f64)
            .num(height as f64);
        let frame = renderer.exchange(request)?;
        Some((renderer, frame))
    }

    /// Whether this renderer has already failed once (a timeout, or the
    /// child's own pipe closing) and so is not worth calling again — the
    /// tab needs a whole new one, which is `app.rs`'s call, not this one's.
    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// A timeout or a broken pipe: stop asking this process anything else,
    /// and kill it outright rather than let a hung script keep spending CPU
    /// in the background for a tab that has already given up on it. Not
    /// waited on here — that risks blocking the very call that just decided
    /// this renderer cannot be waited on; `Drop` reaps it instead.
    fn mark_dead(&mut self) {
        self.dead = true;
        let _ = self.child.kill();
    }

    pub fn click(&mut self, node_id: usize) -> Option<Frame> {
        self.send(Msg::new("click").num(node_id as f64))
    }

    pub fn focus(&mut self, node_id: usize) -> Option<Frame> {
        self.send(Msg::new("focus").num(node_id as f64))
    }

    pub fn blur(&mut self) -> Option<Frame> {
        self.send(Msg::new("blur"))
    }

    pub fn insert_text(&mut self, text: &str) -> Option<Frame> {
        self.send(Msg::new("insert_text").text(text))
    }

    pub fn backspace(&mut self) -> Option<Frame> {
        self.send(Msg::new("backspace"))
    }

    pub fn resize(&mut self, width: f32, height: f32) -> Option<Frame> {
        self.send(Msg::new("resize").num(width as f64).num(height as f64))
    }

    pub fn find(&mut self, query: Option<&str>) -> Option<Frame> {
        self.send(Msg::new("find").text(query.unwrap_or("")))
    }

    /// Put the cursor over an element, for `:hover`, or over nothing.
    pub fn hover(&mut self, node_id: Option<usize>) -> Option<Frame> {
        self.send(Msg::new("hover").num(node_id.map_or(-1.0, |id| id as f64)))
    }

    /// Submit the focused field's form, as Enter in it does, and blur it.
    /// The reply's [`Frame::submission`] carries where to navigate, if the
    /// field was in a form at all.
    pub fn submit(&mut self) -> Option<Frame> {
        self.send(Msg::new("submit"))
    }

    /// The document's readable text and heading outline, for the AI panel —
    /// a pure read with nothing to render, so it ends on `text_content`
    /// rather than `frame`.
    pub fn page_text(&mut self) -> Option<(String, Vec<(u8, String)>)> {
        if self.dead {
            return None;
        }
        wire::write(&mut self.stdin, &Msg::new("page_text")).ok()?;
        loop {
            let Ok(message) = self.replies.recv_timeout(REPLY_TIMEOUT) else {
                self.mark_dead();
                return None;
            };
            match handle_service_message(message, &mut self.stdin, self.loader.as_ref(), Some(self.store.as_ref())) {
                Service::Text(text, headings) => return Some((text, headings)),
                Service::Continue => continue,
                Service::Frame(_) | Service::Broken => {
                    self.mark_dead();
                    return None;
                }
            }
        }
    }

    fn send(&mut self, request: Msg) -> Option<Frame> {
        if self.dead {
            return None;
        }
        self.exchange(request)
    }

    /// Write one request and drive the exchange through to its `frame`
    /// reply, answering whatever the renderer asks along the way — the
    /// interactive-tab counterpart to [`round_trip`], reading from the
    /// background reader thread with [`REPLY_TIMEOUT`] instead of blocking
    /// on the pipe directly.
    fn exchange(&mut self, request: Msg) -> Option<Frame> {
        if wire::write(&mut self.stdin, &request).is_err() {
            self.mark_dead();
            return None;
        }
        loop {
            let Ok(message) = self.replies.recv_timeout(REPLY_TIMEOUT) else {
                self.mark_dead();
                return None;
            };
            match handle_service_message(message, &mut self.stdin, self.loader.as_ref(), Some(self.store.as_ref())) {
                Service::Frame(frame) => return Some(frame),
                Service::Continue => continue,
                Service::Text(..) | Service::Broken => {
                    self.mark_dead();
                    return None;
                }
            }
        }
    }
}

/// Read every message the child sends and forward it to whichever call is
/// waiting — the one background thread that makes [`TabRenderer`]'s calls
/// timeout-bounded instead of blocking on the pipe forever. Ends on its own
/// once the pipe closes (the child died or was killed) or the receiving end
/// is dropped (the `TabRenderer` itself was dropped); neither needs anything
/// from this thread afterward, so nothing signals it to stop early.
fn spawn_reader(mut stdout: ChildStdout) -> mpsc::Receiver<Msg> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(Some(message)) = wire::read(&mut stdout) {
            if tx.send(message).is_err() {
                break;
            }
        }
    });
    rx
}

impl Drop for TabRenderer {
    fn drop(&mut self) {
        // A closed or replaced tab gets no graceful goodbye — kill is
        // simpler than the close-stdin-then-wait a one-shot caller uses, and
        // there is nothing left for a discarded renderer to finish.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
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

/// A stand-in for [`TabRenderer`] used only by `app.rs`'s own unit tests.
///
/// Those tests build a [`crate::app::Tab`] to exercise chrome logic (menus,
/// settings, tab search) that has nothing to do with page content — but
/// `TabRenderer::spawn` has to re-invoke this binary as a child process, and
/// under `cargo test`'s *unit* test harness (unlike an integration test)
/// there is no real `zero` executable to point at: `current_exe()` returns
/// the test harness itself, which has no `--render-worker` handling to
/// re-enter. Rather than give every chrome test a real subprocess to manage,
/// this runs the same [`zero_engine::Document`] calls in-process — same
/// inherent method names and signatures as `TabRenderer`, so `Tab` can hold
/// either behind one `cfg`-selected type alias without its own call sites
/// knowing which one they have.
#[cfg(test)]
pub struct FakeRenderer {
    engine: Engine,
    doc: zero_engine::Document,
    loader: std::rc::Rc<dyn ResourceLoader>,
    width: f32,
    height: f32,
    pending_submission: Option<zero_engine::Submission>,
    click_handled: bool,
}

#[cfg(test)]
impl FakeRenderer {
    pub fn spawn(
        html: &str,
        css: &str,
        width: f32,
        height: f32,
        loader: std::rc::Rc<dyn ResourceLoader>,
        store: std::rc::Rc<dyn zero_engine::KeyValueStore>,
    ) -> Option<(FakeRenderer, Frame)> {
        let doc = zero_engine::Document::load_hosted(html, css, Some(loader.clone()), Some(store));
        let mut me = FakeRenderer {
            engine: Engine::shapes_only(),
            doc,
            loader,
            width,
            height,
            pending_submission: None,
            click_handled: false,
        };
        let frame = me.snapshot();
        Some((me, frame))
    }

    /// Always alive: nothing here can time out or have its pipe close, so
    /// `app.rs`'s crash-recovery path (`Tab::respawn`, `render_pane`'s
    /// `is_dead` check) never has anything to do in a test.
    pub fn is_dead(&self) -> bool {
        false
    }

    pub fn click(&mut self, node_id: usize) -> Option<Frame> {
        self.doc.blur();
        self.click_handled = self.doc.focus(node_id) || self.doc.click(node_id);
        Some(self.snapshot())
    }

    pub fn focus(&mut self, node_id: usize) -> Option<Frame> {
        self.doc.focus(node_id);
        Some(self.snapshot())
    }

    pub fn blur(&mut self) -> Option<Frame> {
        self.doc.blur();
        Some(self.snapshot())
    }

    pub fn insert_text(&mut self, text: &str) -> Option<Frame> {
        self.doc.insert_text(text);
        Some(self.snapshot())
    }

    pub fn backspace(&mut self) -> Option<Frame> {
        self.doc.backspace();
        Some(self.snapshot())
    }

    pub fn resize(&mut self, width: f32, height: f32) -> Option<Frame> {
        self.width = width;
        self.height = height;
        Some(self.snapshot())
    }

    pub fn find(&mut self, query: Option<&str>) -> Option<Frame> {
        self.doc.set_find(query.filter(|q| !q.is_empty()).map(str::to_string));
        Some(self.snapshot())
    }

    pub fn hover(&mut self, node_id: Option<usize>) -> Option<Frame> {
        self.doc.set_hover(node_id);
        Some(self.snapshot())
    }

    pub fn submit(&mut self) -> Option<Frame> {
        self.pending_submission = self.doc.focused_node().and_then(|id| self.doc.submit(id));
        self.doc.blur();
        Some(self.snapshot())
    }

    pub fn page_text(&mut self) -> Option<(String, Vec<(u8, String)>)> {
        Some((self.doc.page_text(), self.doc.headings()))
    }

    /// Render the current document and pack it into the same [`Frame`] shape
    /// `decode_frame` builds from the wire — no bytes cross a pipe here, but
    /// every field means the same thing either way.
    fn snapshot(&mut self) -> Frame {
        let page = self.engine.render_document(&mut self.doc, self.width, self.height, self.loader.as_ref());
        let mut pixels = Vec::with_capacity(page.canvas.pixels.len() * 4);
        for p in &page.canvas.pixels {
            pixels.extend_from_slice(&[p.r, p.g, p.b, p.a]);
        }
        Frame {
            title: self.doc.title(),
            width: page.canvas.width,
            height: page.canvas.height,
            pixels,
            uses_hover: page.uses_hover,
            animating: page.animating,
            is_focused: self.doc.is_focused(),
            element_rects: page.element_rects,
            links: page.links,
            find_matches: page.find_matches,
            submission: self.pending_submission.take(),
            click_handled: std::mem::take(&mut self.click_handled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoStore;
    impl zero_engine::KeyValueStore for NoStore {
        fn get(&self, _: &str) -> Option<String> {
            None
        }
        fn set(&self, _: &str, _: &str) {}
        fn remove(&self, _: &str) {}
        fn clear(&self) {}
    }

    #[test]
    fn a_process_that_can_never_answer_is_given_up_on_promptly_not_after_the_full_timeout() {
        // `TabRenderer::spawn` resolves its executable via `renderer_exe`,
        // which — inside a *unit* test, unlike an integration test — has no
        // real `zero` binary to fall back to (see that function's doc
        // comment) and so spawns this very test binary instead. Handed
        // `--render-worker`, an argument only the production `main` dispatch
        // understands, it exits almost at once having written nothing to
        // its stdout: a closed pipe with no `frame` ever sent, which is
        // exactly the shape of "this process is never coming back." The
        // question this test asks is not "does spawning fail" — it does,
        // trivially — but "how fast does `TabRenderer` notice": a closed
        // pipe has to be caught immediately, not by sitting out
        // `REPLY_TIMEOUT` for a reply that was never going to arrive.
        let loader: std::rc::Rc<dyn ResourceLoader> = std::rc::Rc::new(zero_engine::resource::NullLoader);
        let store: std::rc::Rc<dyn zero_engine::KeyValueStore> = std::rc::Rc::new(NoStore);
        let start = std::time::Instant::now();
        let result = TabRenderer::spawn("<div></div>", "", 100.0, 100.0, loader, store);
        assert!(result.is_none(), "a process that never sends a frame should not produce a renderer");
        assert!(
            start.elapsed() < REPLY_TIMEOUT / 2,
            "a closed pipe should be noticed almost immediately, not by waiting out the timeout"
        );
    }

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
