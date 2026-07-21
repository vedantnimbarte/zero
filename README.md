<p align="center">
  <img src="logo.png" alt="Zero Browser logo" width="160">
</p>

# Zero Browser

> India's first ground-up web browser — engine, shell, and AI, built from scratch in Rust.

**Status:** Pre-development. This repository currently contains the product and
engineering specification only. No application code yet.

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
cargo run -- --png examples/x.html out.png # render just the page
cargo run -- --png page.html out.png rust  # …and highlight a find-in-page query
cargo run -- --ai https://…                # print the on-device page summary
cargo run -- --history                     # dump stored history
```

**Keys:** `Ctrl+T` new tab · `Ctrl+W` close · `Ctrl+Tab` next · `Ctrl+L` clear address ·
`Ctrl+F` find · `Ctrl+D` bookmark · `Ctrl+H` history · `Ctrl+B` bookmarks ·
`Ctrl+I` AI panel · `Alt+←/→` back and forward.

**Built-in pages:** `zero://newtab`, `zero://history`, `zero://bookmarks`.

## What works today

The engine renders real sites — Hacker News and DuckDuckGo results render close to
correctly, including their own stylesheets, tables and forms.

- **HTML**: tolerant parser, character references, raw-text and void elements
- **CSS**: external `<link>` sheets and `@import`, `@media` (type + width), descendant/child
  selectors, custom properties (`var()`, defined on `:root`), the cascade with
  specificity, HTML presentation attributes (`bgcolor`, `width`, `align`),
  named colours, `rgb()`/`hsl()`, alpha
- **Layout**: block, inline, inline-block, flex (wrap/grow/justify/align), grid
  (`repeat()`, `fr`, `minmax()`, spans, named areas), tables (colspan/rowspan),
  out-of-flow positioning, intrinsic sizing, `text-align`
- **Text**: shaping via HarfBuzz with a font fallback chain — Latin, Indic
  (Devanagari, Tamil, Telugu, Bengali and more) and CJK
- **JavaScript**: own lexer, parser and interpreter — closures, classes with
  `super`, `try/catch/finally`, `setTimeout`, DOM query and mutation, events
- **Browser**: vertical tabs, session restore, history, bookmarks, find-in-page,
  form submission and search, an on-device page assistant
- **Privacy**: tracker/ad filtering (Adblock syntax), HTTPS-first, cookies and
  `localStorage` partitioned per site, profile data encrypted at rest (DPAPI on
  Windows; macOS and Linux backends are still to come)

**Known limits.** Pseudo-classes other than `:root` (so `:hover`), attribute
selectors are skipped — a rule using one is dropped rather than
misapplied. Wikipedia's skin renders its columns correctly at
wide window sizes but still overlaps its floating tools rail. Layout and paint are
single-threaded, and a page is painted in full rather than by viewport.

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
