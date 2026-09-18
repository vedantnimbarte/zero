# Zero Browser — Architecture & Engine Specification

**Version:** 0.2
**Last updated:** 2026-09-19
**Scope:** System architecture, from-scratch rendering/JS/network stack, process
and security model, Rust crate map, performance budget.

> **How to read this document.** It describes Zero **as built**, and says so
> plainly where something is not built. Each section separates two things:
>
> * **As built** — what the code in this repository does today. Verifiable by
>   reading it; if it disagrees with the code, the document is wrong.
> * **Intended** — where the section is going. Not a promise of date, and not a
>   description of anything that exists.
>
> Version 0.1 mixed the two, and the engine drifted from it for months without
> anyone noticing: it specified `wgpu` compositing, `tokio` async I/O and
> nineteen crates, none of which were built. The separation above is the fix.
> **Section numbers are referenced from source comments** (`§3`, `§4` and `§10`
> are cited in `css.rs`, `html.rs`, `layout.rs`, `paint.rs`, `js/mod.rs` and
> `js/interp.rs`) — renumbering silently breaks those citations, so sections are
> rewritten in place, never reordered.

---

## 1. Design tenets

1. **Memory-safe by construction** — Rust everywhere; `unsafe` isolated, audited, minimized.
2. **Process isolation** — a compromised web page must not read another site's data or the OS.
3. **Layered and swappable** — the engine knows nothing about windows, tabs or
   files; the shell is one embedder among possible others. This boundary is real
   today and is the one that matters.
4. **Measure before optimizing** — the pipeline is CPU-rastered and
   single-threaded. GPU compositing is not a tenet, it is a candidate: at
   1280x900 a real page takes 25 ms to render and 1.5 ms to composite, so moving
   the composite to the GPU cannot touch the number that dominates. It becomes
   worth doing when rasterization itself moves.
5. **Honest phasing** — the engine implements a defined web subset, and says which.
6. **Every shortcut names its ceiling** — deliberate simplifications carry a
   `ponytail:` comment stating the limit and the upgrade path, so a shortcut is a
   tracked debt rather than a surprise.

---

## 2. Process model

### As built

Two kinds of process:

```
┌──────────────────────────────────────────────────────────────┐
│  Browser process — trusted                                   │
│  • Window, vertical tab rail, toolbar, menus, split view     │
│  • Tab and session orchestration                             │
│  • Network (fetch, TLS, cookies, tracker blocking)           │
│  • Storage (history, bookmarks, downloads, settings)         │
│  • AI (extractive page summarizer)                           │
│  • Startup mitigations applied to itself                     │
└───────────────┬──────────────────────────┬───────────────────┘
                │ pipe IPC (stdin/stdout)  │
        ┌───────▼────────┐        ┌────────▼───────┐
        │ Renderer proc  │  ...   │ Renderer proc  │   (one per TAB)
        │ • HTML/CSS     │        │                │
        │ • Style/Layout │        │                │
        │ • Paint        │        │                │
        │ • JS           │        │                │
        │ NO network     │        │ NO network     │
        └────────────────┘        └────────────────┘
```

- **One renderer per tab.** Page content — the most hostile input the browser
  handles — is parsed, run and painted in a child process that drops what
  privileges it can before reading a byte.
- **The renderer has no network of its own.** Every subresource it needs travels
  back up the pipe for the parent to fetch, which keeps cookies, the tracker list
  and the profile key on the trusted side.
- **IPC** is length-prefixed binary messages over the child's stdin/stdout
  (`wire.rs`). A reader thread feeds replies to a channel so a hung page cannot
  block the UI thread; a reply timeout bounds the wait, and a dead renderer is
  noticed and respawned rather than taking the window down.
- **A spare renderer is kept warm**, so opening a tab does not wait for a process
  to start and read fonts.
- **Navigation between two `zero://` screens reuses the process** — a settings
  toggle is a navigation, and it should not cost a process.
- **Startup mitigations** (`sandbox.rs`) apply to every process: no dynamic code,
  no injected libraries, strict handle checks. **Windows only.**

### Not built

- **Site isolation.** Renderers are keyed by *tab*, not by site (eTLD+1).
- **A filesystem jail.** The renderer drops privileges on its token but is not
  confined by a restricted token, AppContainer, Seatbelt or seccomp-bpf. The
  mitigations make a memory-safety bug harder to escalate; they do not make one
  harmless.
- **Separate service processes.** Network, storage and AI are modules inside the
  browser process, not sandboxed services.
- **Cross-site iframes in their own process** — `<iframe>` is not implemented at all.

### Intended

Site isolation and a per-OS filesystem jail are the next real steps. Splitting
network, storage or AI into service processes buys little until there is a second
consumer or a jail to put them behind.

---

## 3. Rendering pipeline (the from-scratch engine)

The engine turns bytes into pixels. The stage numbers `[1]`–`[7]` below are cited
from source comments and are stable.

```
Network bytes
   │
   ▼
[1] HTML tokenizer + tree builder ──► DOM tree
   │                                    │
   │                                    ▼
   │                             [3] Style engine
   ▼                              (CSS parse + cascade + match)
[2] CSS parser ──► Stylesheets ─────────┘
                                        │
                                        ▼  styled tree (computed values)
                              [4] Box tree construction
                                        │
                                        ▼
                              [5] Layout (block, inline, flex, grid, table)
                                        │  → geometry (x,y,w,h per box)
                                        ▼
                              [6] Paint → display list → CPU raster
                                        │
                                        ▼
                              [7] Present (shell blits the canvas)
```

**[1] HTML parsing** (`html.rs` → `dom.rs`)
*As built:* a tolerant recursive-descent parser. Doctypes, comments, void
elements, self-closing tags, raw-text elements (`<script>`, `<style>`), unquoted
and boolean attributes, mismatched close tags — recovering rather than panicking.
*Not built:* WHATWG tree construction. No implied tags (no auto `<tbody>`, no
`<p>` auto-close), no adoption-agency error recovery, no full entity table, no
fragment parsing.
*Intended:* grow toward the WHATWG state machine, measured against a conformance
corpus rather than by inspection.

**[2] CSS parsing** (`css.rs`)
*As built:* selectors (tag, `#id`, `.class`, `*`, descendant, `>`, `+`, `~`),
attribute selectors, structural and state pseudo-classes, declarations, `@media`
(type and width), `@import`, custom properties, `calc()` as a deferred expression
resolved at use, shorthand expansion, `rgb()`/`hsl()`/named colours, lengths in
px/%/em/rem, `@font-face`, the `style` attribute, and `::before`/`::after` as
pseudo-elements whose declarations are kept aside for the box they generate.
Unknown rules and values are skipped, never fatal.
*Not built:* `:has()` — a rule carrying it is dropped whole rather than
misapplied. No `@supports` or `@keyframes`. The `font` **shorthand** is not
expanded, so a page that sets its face that way gets none of it.

**[3] Style engine** (`style.rs`)
*As built:* a `RuleIndex` buckets rules by tag, class and id, so matching tests
candidate rules rather than the whole stylesheet. Cascade with specificity and
source order, inheritance, and custom-property resolution.
*Not built:* **invalidation.** Any change restyles the whole document; there is no
dirty-subtree tracking.

**[4]–[5] Layout** (`layout.rs`)
*As built:* one `LayoutBox` tree built from the styled tree and mutated in place.
Block, inline, inline-block, flex (wrap, grow, justify, align), grid (`repeat()`,
`fr`, `minmax()`, spans, named areas), tables (colspan/rowspan), floats and
`clear`, intrinsic sizing, and positioning: `relative`, `absolute` against the
nearest positioned ancestor, `fixed` against the viewport, and `sticky` as a
post-pass once the tree is laid out (which is why `Page::uses_sticky` exists —
it tells the embedder that this page's band does not survive a scroll).
*Not built:* sticky honours `top` only and is not clamped to its containing
block, so it stays pinned past the end of the section it belongs to; `fixed` is
anchored to the viewport but does not ride the window as you scroll. Inline
layout is word-granular, splitting on ASCII whitespace.

**[6] Paint** (`paint.rs`)
*As built:* walk the box tree into a display list (solid and rounded rects,
gradients, shadows, borders, shaped text runs, images, clips), then rasterize on
the CPU into a canvas of RGBA pixels. Anti-aliased text, `opacity`, `overflow`
clipping and `transform: translate/scale` are applied as the list is built. A
text run carries its clip rather than being kept or dropped whole, so `overflow`
cuts words at the box edge — which is what makes a box collapsed to a pixel, the
way every large site hides a screen-reader heading, actually hide it.
*Not built:* the display list is **flat** — `z-index` orders siblings rather than
establishing stacking contexts, and `opacity` applies per command rather than to a
composited layer. Within one `z-index`, positioned boxes do at least paint after
non-positioned ones, which is the part of CSS's painting order overlays need.

**[7] Present** (`zero-shell`)
*As built:* the shell blits the finished canvas to the window via `softbuffer`.
There is no compositor, no layerization and no GPU path. Measured at 1280x900:
25 ms to render a real page, 1.5 ms to composite the frame.

**Supporting modules:** `text.rs` (shaping via `rustybuzz`, rasterizing via
`fontdue`, per-word font fallback across a priority chain, and `font-family`
matching by the name a face gives itself), `woff2.rs` (a page's own `.woff2`
typefaces unpacked back into an sfnt), `svg.rs` (Zero's own
SVG rasterizer — shapes, paths, fills, strokes, `viewBox` — used for both
`<img src=".svg">` and inline `<svg>`), `anim.rs` (CSS transitions, holding the
previous value per element against a document clock), `resource.rs` (image decode).

### Decisions this document never stated

Four properties of the pipeline arrived with the first engine commit as defaults
rather than choices, and were never written down. They are recorded here because
each carries a ceiling, and the cost of changing each grows with the layout code:

| Default | Ceiling it imposes | Cost to change |
|---|---|---|
| Coordinates are `f32` | Accumulated float drift; positions that should compare equal do not. Makes pixel reftests flaky in ways that look like layout bugs | Low — mechanical, ~230 sites |
| Geometry is **physical only** (`left`/`right`/`top`/`bottom`) | `direction: rtl` and `writing-mode: vertical-*` are unreachable. Blocks Urdu and Japanese vertical text | **Rises fastest.** ~220 sites in `layout.rs` today, plus threading a writing mode through every containing block |
| One **mutable** box tree, rebuilt per layout | No incremental layout and no parallel layout. Every layout is full-page | High — restructures layout. No payoff until incremental layout is real |
| Computed values are `HashMap<String, Value>` | Every property read hashes a string; no sharing between elements that computed identically | Moderate — 87 call sites across 55 distinct properties |

*Intended:* logical geometry is the one with a closing window and should land
before the layout code grows further. Fixed-point coordinates and typed computed
values are cheap whenever. The box/fragment-tree split waits for incremental
layout to be worth having.

---

## 4. JavaScript engine (`zero-engine/src/js/`)

### As built

Zero's own lexer, parser and **tree-walking interpreter** — no third-party JS
engine. This is phase 1 of the plan below, delivered: correctness over speed.

Supported: closures, classes with `super`, `try`/`catch`/`finally`, regex
literals (own engine), `setTimeout`, `JSON`, `Promise` with `then`/`catch`/
`finally`/`all`, `async`/`await`, `fetch`, `localStorage`, `console`, DOM query
(`getElementById`, `getElementsByTagName`, `getElementsByClassName`,
`querySelector`, `querySelectorAll`), DOM mutation (`innerHTML`, `innerText`,
`textContent`, `className`, `value`), and event listeners.

Performance work has been measurement-led: the interpreter is 4–18x faster than
its first version, and profiling showed the cost was never the AST walk (see
`crates/zero-engine/examples/jsbench.rs`).

### Not built

The gap is the **standard library**, not only speed. Absent: `Math`, `Date`,
`Object.*`, `Array` methods beyond `push`/`pop`/`join`, most `String` methods,
`setInterval`, `createElement`/`appendChild`, `element.style`, `classList`,
`location`, `history`, `XMLHttpRequest`, ES modules, getters and setters,
generators. Any script written against a modern baseline fails early. There is no
garbage collector — values are reference-counted, and cycles leak.

### Intended

Unchanged in shape from v0.1, and still the right order:

- **Phase 2 — bytecode VM and inline caches:** resolved variable slots,
  shape-based property access. Worth doing when a real page's scripts are what is
  slow.
- **Phase 3 — baseline JIT:** hardened (W^X, guard pages). Years out.
- **GC:** precise mark-sweep before anything generational.

The standard library outranks all of the above: a VM that runs an incomplete
language faster does not run more pages.

> **Reality note:** a from-scratch JS engine will not match V8 for years. The
> strategy is to render Zero's own surfaces and a growing subset of the web on our
> engine, and hand the arbitrary long-tail web to the browser the system already
> has (§11) until the engine catches up.

---

## 5. Networking stack (`zero-shell/src/net.rs`)

### As built

- **Protocol:** HTTP/1.1 over `ureq`, which wraps `rustls` for TLS. One pooled
  agent per process, so a page's subresources reuse an open TLS session instead of
  paying for a handshake each.
- **Identity:** the user-agent names Zero honestly rather than impersonating
  another browser.
- **Cookies** (`cookies.rs`): partitioned per top-level site *and* per space, so a
  profile is a real boundary. Persisted encrypted.
- **Content blocking** (`blocker.rs`): Adblock-syntax filtering at the fetch layer,
  so a blocked request never leaves the machine.
- **HTTPS-first:** plain-HTTP targets are upgraded, and the URL that actually
  loaded is adopted if it changes.
- **Transport encoding:** gzip and deflate, via `ureq`.
- Runs **in the browser process**. The renderer asks it for bytes over the pipe.

### Not built

- **HTTP/2 and HTTP/3.** HTTP/1.1 only.
- **An HTTP cache.** There is a per-page-load in-memory map, but no
  `Cache-Control`, `ETag` or `Expires` handling and nothing on disk — so every
  navigation refetches every subresource.
- **POST.** A form with `method=post` is not submitted at all, which means no
  logins anywhere.
- **A resolver of our own**, and no DoH. **HSTS.** **Mixed-content blocking.**
  **A redirect policy of our own** (delegated to `ureq`).
- **Async I/O.** Fetching is synchronous and blocks the caller.

### Intended

POST first — it is the single largest gap between Zero and a browser someone can
use daily. Then an RFC 9111 cache, then HSTS. HTTP/2 needs an async runtime and is
a larger change than it looks.

---

## 6. Storage (`zero-shell/src/storage.rs`, `spaces.rs`, `crypto.rs`)

### As built

- **Per-space profile directories.** A space *is* a profile: its own tabs, history,
  cookies, `localStorage`, downloads, settings and encryption key.
- **The format is tab-separated lines**, not a database. It needs no dependency, is
  trivially inspectable, and a corrupt line can be skipped instead of failing the
  whole file.
- **Encrypted at rest on all three platforms** — DPAPI on Windows, AES-256-GCM
  under a Keychain or Secret Service key elsewhere.
- **Sync is a file, not a service:** `--export` seals a space under a fresh 32-byte
  code, `--import` opens it elsewhere. The code never leaves the user's hands.
  Import replaces the space's files rather than merging them.

### Not built

No embedded database — `rusqlite` versus `redb` was a v0.1 open question, and flat
files turned out to be enough. No password storage, because there are no logins to
store (§5). No quota management or eviction.

---

## 7. AI (`zero-shell/src/ai.rs`)

### As built

`LocalAssistant` is **not a language model.** It is a classic extractive
summarizer — word-frequency sentence ranking plus document statistics. It ships as
the default because it needs no network, no API key and no consent, which makes it
the only provider that satisfies the privacy invariant in
`docs/04-SECURITY-PRIVACY.md` §5.5 unconditionally.

The page context handed to it is deliberately text-only: URL, text, headings,
tracker count, security state. No markup, no scripts, no cookies, no form values.

### Not built

No model, on-device or cloud. No separate AI process — it is a module in the
browser process. No agentic actions, so no confirmation gate exists yet.

### Intended

A real model plugs in behind the `Assistant` trait and receives the same
`PageContext`. Anything leaving the device is gated on explicit, per-action
consent, and that gate belongs in the shell, never in the engine. Agentic actions
need the capability gate before they need a model.

---

## 8. UI shell and compositing (`zero-shell`)

### As built

- **Window and input** via `winit`; **presenting** via `softbuffer` — a CPU
  framebuffer blitted to the window. No `wgpu`, no GPU path.
- **The chrome is drawn by Zero's own engine.** The tab rail, toolbar, menus and
  the `zero://` pages are small HTML documents with inline `<svg>` icons, put
  through the same pipeline a website goes through. If the browser's own buttons
  look wrong, the engine has a bug worth fixing — which is the point, and it means
  the chrome is a conformance test that runs every frame.
- **The chrome is rebuilt from scratch each frame.** The shell animates the rail
  width itself and asks for the next frame only while something is moving; an idle
  window draws nothing.
- **Settings are links.** Each control is an ordinary anchor carrying its new value
  (`zero://settings?rail=icons`), so changing a preference goes through the same
  navigation path as clicking any link on the web.
- **Headless modes** for review and testing: `--png` renders a page in a renderer
  process; `--shot` screenshots the whole window, with *poses* that put the chrome
  into states a still image cannot otherwise reach.

### Resolved

The v0.1 open question — own `wgpu` UI versus a mature Rust toolkit — was answered
by building a third option: the chrome is HTML rendered by our own engine, on the
CPU. It gave the design system and the engine one implementation instead of two.

---

## 9. Rust crate / workspace map

### As built

**Two crates.**

```
zero/
├── crates/
│   ├── zero-engine     # html, css, style, layout, paint, text, svg, anim, js
│   └── zero-shell      # window, chrome, tabs, spaces, net, storage, crypto,
│                       # blocker, cookies, renderer process, AI, i18n
├── docs/
├── examples/
└── Cargo.toml          # workspace
```

The one boundary that is real is the important one: **the engine knows nothing
about windows, tabs or files.** It takes bytes and a `ResourceLoader` and returns a
canvas. The shell is one embedder among possible others.

A module that has never had a second consumer does not need to be a crate. The
nineteen-crate map in v0.1 described boundaries that had not been earned; crates
split off when a second consumer or a separate build appears, not before.

**Dependencies, and why each is not "embedding an engine":** `winit` and
`softbuffer` (windowing and presenting — shell concerns), `ureq` and `rustls`
(HTTP and TLS math), `fontdue` (TTF rasterizing), `rustybuzz` (shaping — required
for correct Indic rendering, since matra reordering and conjuncts are not
expressible as one glyph per character), `image` (decoding content images),
`allsorts` (WOFF2 decoding — the container is Brotli-compressed and its
`glyf`/`loca` tables are transformed and have to be rebuilt; only the decode is
borrowed, and packing the result back into an sfnt is ours), `aes-gcm` and
`windows-sys` (encryption at rest).

The line these imply, and the one worth holding: **engine algorithms are ours; OS
and protocol plumbing can be borrowed.** A compression format is plumbing; a
layout algorithm is not.

---

## 10. Web-platform support

### As built

| Capability | State |
|---|---|
| HTML parsing | Tolerant, not WHATWG. No implied tags, no adoption agency, no `<template>` parsing — a template's children land in the DOM and are held back by the UA stylesheet rather than by the parser |
| HTML `hidden` attribute | Supported, via the UA stylesheet |
| CSS box model, colours, backgrounds, borders | Supported |
| CSS selectors: descendant, child, sibling, attribute, structural | Supported |
| CSS `::before`/`::after` | Content generated; the box inherits the element's styling rather than taking the rule's own |
| CSS `:has()` | **Not supported** — the rule is dropped whole |
| CSS inline `style` attribute | Supported, last in the cascade (`!important` is not modelled) |
| CSS flexbox | Supported (wrap, grow, justify, align) |
| CSS grid | Supported (`repeat()`, `fr`, `minmax()`, spans, named areas) |
| CSS tables, floats, `clear`, absolute and relative positioning | Supported |
| CSS `position: sticky` | Supported, `top` only and unclamped. `fixed` anchors to the viewport but does not ride a scroll |
| CSS transforms (translate/scale), transitions, `opacity`, `z-index` | Supported. `z-index` orders siblings only |
| CSS custom properties, `calc()`, `@media`, `@import` | Supported |
| CSS `@font-face`, `font-family` | Supported, `.woff2` included, plus the `serif`/`monospace` generics. The `font` shorthand is not expanded |
| CSS `text-overflow: ellipsis`, `white-space: nowrap` | Supported |
| Text: Latin and Indic shaping, font fallback | Supported; a run is clipped per glyph by `overflow` |
| Text: line breaking | **ASCII whitespace only** — CJK and Thai never wrap |
| Text: bidi (RTL) | **Not supported** — Urdu renders backwards |
| Text selection and copy | Supported |
| Images: PNG, JPEG, GIF, WebP, BMP, ICO | Supported |
| SVG | Own rasterizer: shapes, paths, fills, strokes, `viewBox` |
| JS: interpreter | Supported, minus most of the standard library (§4) |
| JS: bytecode VM, JIT | Not built |
| Web APIs: `fetch`, DOM query and mutation, events, `localStorage` | Supported |
| Forms: GET submission, `<select>`, text input, IME | Supported |
| Forms: POST | **Not supported** — no logins |
| `<iframe>` | Not supported |
| `<video>` / `<audio>` | Not supported |
| Accessibility tree | Not supported — invisible to a screen reader |
| Cross-site iframe process isolation | Not applicable until iframes exist |

### Intended

The ordered work is tracked as **GitHub milestones**, not phases — they were set
by opening real sites and writing down what broke, so they lead with what makes
Zero unusable rather than what is furthest from spec. `docs/03-ROADMAP.md` holds
the strategy; the milestones hold the queue.

Each capability should graduate against a **conformance corpus**, not against
inspection. See §14.

---

## 11. The compatibility bridge

### As built

**Not built, and being questioned rather than scheduled.** `Ctrl+Shift+O` hands
the current page to whatever browser the system already has. That keeps the
promise that nobody is stuck on a page Zero renders poorly, and costs one
keystroke instead of shipping, sandboxing and updating a second engine.

It is a *handoff*, not a bridge: the page opens **there**, outside Zero's cookie
jar and tracker blocking, and the user can see that it did.

### Intended

If the handoff proves insufficient in real use, an embedded-engine bridge is the
answer. Until then it is 200 MB of speculation, and the v0.1 plan to ship one
behind a flag is withdrawn rather than deferred.

---

## 12. Performance

### Measured

| What | Number | Notes |
|---|---|---|
| Render a real page at 1280x900 | 25 ms | CPU, single-threaded, whole document rather than viewport |
| Composite that frame | 1.5 ms | 2.6x faster once it stopped doing per-pixel coordinate arithmetic at zoom 1 |
| JS interpreter | 4–18x faster than v1 | `examples/jsbench.rs` |
| New-tab create | Does not wait on process start | A spare renderer is kept warm |

The 25 ms and the 1.5 ms together are why GPU compositing is not scheduled: moving
the 1.5 ms to the GPU cannot touch the 25 ms. Rasterization has to move first.

### Not measured

Cold start, input latency, idle RAM, memory under many tabs. There is **no perf
CI**, so none of these is defended against regression. The v0.1 budgets — ≤1500 ms
cold start, ≤100 ms new tab, ≤16.6 ms frame, ≤50 ms input latency — stand as
targets with nothing enforcing them.

---

## 13. Security model

Summary; the full document is `docs/04-SECURITY-PRIVACY.md`.

### As built

- **Page content runs out of process**, privilege-dropped, with no network of its
  own (§2).
- **Startup mitigations** on every process: no dynamic code, no injected
  libraries, strict handles. Windows only.
- **Cookies and `localStorage` partitioned** per site and per space.
- **Profile data encrypted at rest** on all three platforms.
- **HTTPS-first** upgrade.
- **Rust safety** eliminates whole exploit classes in safe code.

### Not built

Site isolation. A filesystem jail (restricted token, AppContainer, Seatbelt or
seccomp-bpf). HSTS. Mixed-content blocking. Signed updates and rollback protection
— there is no updater. An inventory of `unsafe` blocks.

---

## 14. Testing and CI

### As built

**251 unit tests** across the two crates, plus an integration test for the
renderer process. They are genuine — parser, cascade, layout, JS and wire-format
behaviour, not smoke tests.

### Not built

**There is no CI.** `.github/` holds issue and PR templates and no workflows, so
nothing runs on push: not the test suite, not a build on the other two platforms,
not a lint. No WPT. No Test262. No fuzzing. No screenshot diffing. No perf CI.

This is the largest gap between this document and a project that can safely accept
contributions, and it gates everything in §10 — "renders correctly" is currently an
assertion, not a measurement.

### Intended, in order

1. **CI that runs `cargo test` on three platforms.** Everything else here is
   speculative until this exists.
2. **WPT reftests against `--png`.** WPT's CSS reftests render two documents and
   compare pixels, which the existing headless mode already does — so a meaningful
   conformance number is reachable **without** implementing WebDriver. Vendor only
   the subdirectories under test; the full suite is around 1.1 GB.
3. **Unicode conformance data** (`LineBreakTest.txt`, `BidiTest.txt`) for the text
   work in §3 and §10.
4. **Fuzzing** the HTML, CSS and JS parsers — untrusted input is the top attack
   surface.
5. **Test262 subset** for §4.
6. **Perf CI** enforcing §12.

---

## 15. Key architectural risks

| Risk | Impact | Status |
|------|--------|--------|
| From-scratch engine timeline | High | Live. Mitigated by phasing and the §11 handoff |
| No CI, no conformance measurement | High | **Live and unmitigated.** See §14 |
| JS standard library gap blocks real pages | High | Live. Outranks JS performance work |
| Physical-only geometry blocks RTL and vertical text | Med | Live, and the cost rises with every layout change (§3) |
| No POST means no logins | High | Live. See §5 |
| Cross-platform sandbox parity | Med | Live. Mitigations are Windows-only |
| Indic text shaping correctness | Med | Mitigated — `rustybuzz`, and the chrome exercises it every frame |
| Team scale for a browser | High | Live. OSS contribution model, and §14 is what makes contribution safe |

---

## 16. Open decisions

The v0.1 list is resolved or withdrawn: the shell-renderer question was answered
by building the chrome in our own engine (§8); the embedded-database question
dissolved when flat files proved sufficient (§6); the compat bridge is withdrawn
rather than deferred (§11). What remains open:

1. **Logical geometry — when, not whether.** The cost rises with every layout
   change, and it gates RTL and vertical writing modes (§3).
2. **Licence boundary for borrowed code.** Zero is Apache-2.0. Some ecosystem
   pieces that would save real time are MPL-2.0, which is file-level and
   perpetual. Depending on one is a clean, visible boundary; copying its source in
   is not. Worth deciding deliberately rather than drifting into.
3. **Accessibility: own implementation or OS bindings.** A screen-reader tree
   means UIA, AT-SPI and NSAccessibility — three platforms' APIs, which sit on the
   plumbing side of the §9 line rather than the engine side.
4. **JS GC strategy.** Reference counting leaks cycles today; mark-sweep is the
   next step, and the trigger for doing it is a real page that leaks.
5. **HTTP stack.** An async runtime is the price of HTTP/2, worth paying when
   HTTP/1.1 is measurably the problem.
