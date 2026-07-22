<p align="center">
  <img src="logo.png" alt="Zero Browser logo" width="160">
</p>

# Zero Browser

> India's first ground-up web browser — engine, shell, and AI, built from scratch in Rust.

**Status:** Early alpha. The engine and shell both run — `cargo run` opens a
browser that renders real sites. See [What works today](#what-works-today).

Zero is an open-source, privacy-first, AI-native web browser with a minimal,
spacious UI (inspired by Arc, Dia, and Comet) and **vertical tabs**. Unlike Arc,
Dia, and Comet — which are all Chromium shells — Zero's rendering engine,
JavaScript engine, and networking stack are being written from scratch in Rust.

## Why "from scratch" is a deliberate, phased bet

Building a browser engine is one of the largest efforts in software (Chromium and
Gecko each represent 20+ years and hundreds of engineers). Zero does not pretend
otherwise. Instead of chasing full web-compatibility on day one, Zero ships in
**phases**, each of which is a usable product for a growing slice of the web:

- **Phase 0–1:** Native shell + AI, rendering a controlled subset of the web
  (static content, documents, our own new-tab/AI surfaces) with our own engine.
- **Phase 2–3:** Progressive engine expansion (flexbox/grid, more JS, media).
- **Phase 4+:** Broad real-world site compatibility.

A **compatibility-bridge fallback** (an embedded engine behind a feature flag) is
specified so early adopters are never fully blocked while our engine matures.
See [`docs/03-ROADMAP.md`](docs/03-ROADMAP.md) for the honest timeline and risks.

## Try it

```sh
cargo run                                  # opens the browser, restoring last session
cargo run -- https://news.ycombinator.com  # open a URL
cargo run -- examples/flex.html            # open a local file

cargo run -- --shot https://… out.png      # screenshot the whole window, headless
cargo run -- --shot page.html out.png 1280x800 menu tabs:4   # …posed (see below)
cargo run -- --png examples/x.html out.png # render just the page
cargo run -- --png page.html out.png rust  # …and highlight a find-in-page query
cargo run -- --ai https://…                # print the on-device page summary
cargo run -- --history                     # dump stored history
```

`--shot` takes an optional `WxH` and then any number of **poses**, which put the
chrome into a state a still image cannot otherwise reach — so every surface stays
reviewable without holding the mouse in the right place. Poses never change your
saved settings.

| Pose | Effect |
|------|--------|
| `menu` | the overflow menu, open |
| `ai` | the assistant panel, open |
| `hover:star` | a control lit, with its tooltip |
| `search:wiki` | tab search, filtering |
| `tabs:5` | extra tabs, one of them pinned |
| `split` | two pages side by side |
| `space:work` | a different profile, with its own accent |
| `railpx:150` | the tab rail caught mid-slide |
| `layout=horizontal`, `rail=icons`, `zoom=150`, … | any setting |

**Keys:** `Ctrl+T` new tab · `Ctrl+Shift+T` reopen closed · `Ctrl+W` close ·
`Ctrl+Tab` next · `Ctrl+Shift+A` search tabs · `Ctrl+\` collapse the tab rail ·
`Ctrl+L` clear address · `Ctrl+F` find · `Ctrl+D` bookmark · `Ctrl+S` save page ·
`Ctrl+H` history · `Ctrl+B` bookmarks · `Ctrl+J` downloads · `Ctrl+,` settings ·
`Ctrl+U` view source · `Ctrl+R` reload · `Ctrl+I` AI panel ·
`Ctrl+=`/`Ctrl+-`/`Ctrl+0` zoom (also `Ctrl`+wheel) · `Alt+←/→` back and forward.

**Built-in pages:** `zero://newtab`, `zero://history`, `zero://bookmarks`,
`zero://downloads`, `zero://settings`.

**Spaces** are separate profiles in one browser — their own tabs, history,
cookies, `localStorage`, downloads, settings and encryption key, because a space
*is* a profile directory. `zero://settings?space=work` makes one and switches to
it; the accent colour changes with it, so you can see which profile you are
typing into.

**Settings** live at `zero://settings` — tab layout (vertical rail or a horizontal
strip), how far the rail collapses, page zoom, interface language (English or
हिन्दी), search engine, tracker blocking, session restore and animation. Each control is an ordinary link carrying its new
value (`zero://settings?rail=icons`), so changing a preference goes through the
same navigation path as clicking any link on the web.

The rail slides open and closed rather than snapping. The engine has no CSS
transitions, so the shell animates the width itself and asks for the next frame
only while something is moving — an idle window draws nothing. There is no OS
reduced-motion signal available here, so **Animation: Off** in settings is offered
directly for anyone who wants the change to be instant.

## What works today

The engine renders real sites — Hacker News, DuckDuckGo results and Wikipedia
articles render close to correctly, including their own stylesheets, tables and
forms.

- **HTML**: tolerant parser, character references, raw-text and void elements
- **CSS**: external `<link>` sheets and `@import`, `@media` (type + width),
  descendant/child/sibling selectors (`div p`, `>`, `+`, `~`),
  attribute selectors (`[type=text]`, `~=`, `^=`, `$=`, `*=`),
  pseudo-classes (`:hover`, `:nth-child()`, `:first-child`, `:not()`, `:checked`),
  `visibility`, `opacity`, `z-index`, `transform: translate`,
  `overflow` clipping, custom properties (`var()`, defined on `:root`), the cascade with specificity, HTML presentation attributes (`bgcolor`, `width`, `align`),
  named colours, `rgb()`/`hsl()`, alpha
- **Layout**: block, inline, inline-block, flex (wrap/grow/justify/align), grid
  (`repeat()`, `fr`, `minmax()`, spans, named areas), tables (colspan/rowspan),
  floats and `clear`, out-of-flow positioning, intrinsic sizing, `text-align`,
  `white-space: pre`
- **Text**: shaping via HarfBuzz with a font fallback chain — Latin, Indic
  (Devanagari, Tamil, Telugu, Bengali and more) and CJK
- **JavaScript**: own lexer, parser and interpreter — closures, classes with
  `super`, `try/catch/finally`, regex literals, `setTimeout`, `JSON`, `fetch`
  with promises and `await`, DOM query and mutation, events
- **Browser**: vertical tabs, split view, spaces (separate profiles), session restore, history, bookmarks, find-in-page,
  form submission and search, an on-device page assistant, an English/Hindi
  interface, and input-method text so Indic scripts can be typed
- **Privacy**: tracker/ad filtering (Adblock syntax), HTTPS-first, cookies and
  `localStorage` partitioned per site, profile data encrypted at rest on all
  three platforms — DPAPI on Windows, AES-256-GCM under a Keychain or Secret
  Service key elsewhere

**Known limits.** A pseudo-class the engine cannot honour exactly (`::before`,
`:has()`) still takes its rule with it, rather than being misapplied. Text does
not re-widen below a short float — a block beside one keeps its narrowed width
for its whole height. Layout and paint are single-threaded, and a page is
painted in full rather than by viewport.

## The documents

| Doc | What it covers |
|-----|----------------|
| [`docs/00-PRD.md`](docs/00-PRD.md) | Product vision, users, goals, features, success metrics |
| [`docs/01-ARCHITECTURE.md`](docs/01-ARCHITECTURE.md) | System architecture + from-scratch engine spec (crate map, pipeline, process model) |
| [`docs/02-UI-UX-SPEC.md`](docs/02-UI-UX-SPEC.md) | Design system, vertical tabs, split view, screens, motion |
| [`docs/03-ROADMAP.md`](docs/03-ROADMAP.md) | Phased milestones, realistic timeline, risk register |
| [`docs/04-SECURITY-PRIVACY.md`](docs/04-SECURITY-PRIVACY.md) | Threat model, sandboxing, data sovereignty |

## Principles

1. **Privacy is default, not a setting.** No telemetry without opt-in; data stays on-device or in-India.
2. **Minimal & spacious.** The UI gets out of the way. Fewer chrome pixels, more content.
3. **Fast by construction.** Rust, GPU compositing, per-tab process isolation.
4. **AI is a first-class surface**, not a bolted-on sidebar.
5. **Open source**, community-governed, made in India.

## License

Licensed under the [Apache License 2.0](LICENSE).
