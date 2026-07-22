# Zero Browser — Architecture & Engine Specification

**Version:** 0.1 (Draft)
**Last updated:** 2026-07-21
**Scope:** System architecture, from-scratch rendering/JS/network stack, process
and security model, Rust crate map, performance budget.

---

## 1. Design tenets

1. **Memory-safe by construction** — Rust everywhere; `unsafe` isolated, audited, minimized.
2. **Process isolation** — a compromised web page must not read another site's data or the OS.
3. **Layered & swappable** — each engine stage is a crate with a clean interface, so it can grow phase by phase and be tested in isolation.
4. **GPU-first rendering** — composite on the GPU via `wgpu`; CPU does layout, GPU does pixels.
5. **Async I/O** — networking and disk on `tokio`; the UI thread never blocks.
6. **Honest phasing** — the engine implements a defined web subset first; a **compat bridge** exists behind a flag so users aren't blocked (see §11).

---

## 2. Process model

Zero uses a **multi-process** architecture, like every serious browser, because a
single-process browser cannot be secure. One page crash or exploit must be contained.

```
┌──────────────────────────────────────────────────────────────┐
│  Browser (main) process — trusted                            │
│  • UI shell, vertical tabs, window mgmt (native GUI)         │
│  • Tab/session orchestration, IPC broker                    │
│  • Network service, storage service, AI service (see below) │
└───────────────┬───────────────────────────┬─────────────────┘
                │ IPC (typed, capability)    │
        ┌───────▼───────┐           ┌────────▼────────┐
        │ Renderer proc │   ...     │ Renderer proc   │   (1 per site/tab,
        │ (sandboxed)   │           │ (sandboxed)     │    site-isolated)
        │ • HTML/CSS    │           │                 │
        │ • Layout/Paint│           │                 │
        │ • JS engine   │           │                 │
        └───────────────┘           └─────────────────┘

  Out-of-process services (sandboxed, one each):
   • Network service   • Storage service   • GPU/compositor service
   • AI service        • Extension host (v1: limited)
```

- **Site isolation:** renderer processes are keyed by site (eTLD+1). Cross-site
  iframes can be moved to their own process (Phase 3+; single-process-per-tab in early phases).
- **Sandboxing:** renderers run with the OS's strongest available sandbox
  (Windows: AppContainer/job objects + Win32k lockdown; macOS: Seatbelt/App Sandbox;
  Linux: seccomp-bpf + user namespaces). Renderers have **no direct** file, network,
  or GPU access — everything goes through brokered IPC to services.
- **IPC:** typed messages over an async channel; capability-scoped (a renderer can
  only request resources for its own tab). Serialization via a compact binary format.

---

## 3. Rendering pipeline (the from-scratch engine)

The engine turns bytes on the wire into pixels on screen. Stages, each a crate:

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
                                        ▼  styled DOM (computed styles)
                              [4] Box tree / layout tree construction
                                        │
                                        ▼
                              [5] Layout (block, inline, flex, grid)
                                        │  → geometry (x,y,w,h per box)
                                        ▼
                              [6] Paint → display list
                                        │
                                        ▼
                              [7] Compositor (layerize, raster, GPU present)
```

**[1] HTML parsing** (`zero-html`)
- Spec-conformant tokenizer + tree construction (WHATWG HTML). This part *must* be
  faithful — malformed HTML handling is where "from scratch" engines usually break.
- Produces a DOM with the standard node types; exposes a DOM API to JS (via bindings).

**[2] CSS parsing** (`zero-css`)
- Tokenizer + parser for selectors, declarations, at-rules. Build a stylesheet model.
- Phase-gated property support (see §10 roadmap alignment).

**[3] Style engine** (`zero-style`)
- Selector matching, the cascade, specificity, inheritance, computed values.
- Invalidation: recompute only affected subtrees on DOM/style mutation.

**[4]–[5] Layout** (`zero-layout`)
- Box generation from styled DOM; the layout tree.
- Layout algorithms added in phases: **block & inline first**, then **flexbox**,
  then **grid**. Text layout needs proper line breaking + **Indic script shaping**
  (via HarfBuzz-class shaping — we may use `rustybuzz` for shaping, still our engine).
- Fonts: system font enumeration + `.ttf/.otf` parsing (`ttf-parser`-class), fallback chains.

**[6] Paint** (`zero-paint`)
- Walk layout tree → build a **display list** (ordered draw ops: rects, text runs,
  images, borders, shadows, clips). No pixels yet — a resolution-independent list.

**[7] Compositor** (`zero-compositor`)
- Assign content to GPU layers; rasterize display-list tiles; composite on GPU via
  `wgpu`. Handles scrolling, transforms, opacity as compositor operations for 60fps.
- Runs in the GPU service process; renderers submit display lists over IPC.

---

## 4. JavaScript engine (`zero-js`)

The single hardest component. Realistic phased approach:

- **Phase 1 — Interpreter:** a correct, tree-walking / bytecode interpreter for a
  conformant ECMAScript subset (ES2015 baseline growing outward). Correctness over speed.
- **Phase 2 — Bytecode VM + inline caches:** register/stack bytecode VM, hidden
  classes / shape-based property access, inline caches. Real-world-usable speed.
- **Phase 3 — Baseline JIT:** template/baseline JIT for hot functions (guarded by
  the security model — JIT pages are a classic exploit surface, handled carefully).
- **Later — Optimizing JIT:** only if/when justified; this is years out.

Supporting pieces:
- **DOM bindings** — the bridge between `zero-js` and `zero-html`/Web APIs.
- **Event loop** — microtask/macrotask queues, timers, `requestAnimationFrame`.
- **Garbage collector** — start with a simple precise mark-sweep; evolve to
  generational as needed. GC is a `ponytail:` known-ceiling area — naive first, upgrade when profiling demands.

> **Reality note:** a from-scratch JS engine will not match V8 for years. The
> product strategy (Roadmap §) is to render *our own* AI/new-tab/document surfaces
> and a growing subset of the web on our engine, and route the arbitrary long-tail
> web through the compat bridge until the engine catches up.

---

## 5. Networking stack (`zero-net` — Network service)

- **Protocols:** HTTP/1.1 and HTTP/2 in v1 (via `hyper`-class or our own on `tokio`);
  HTTP/3/QUIC later. TLS via `rustls` (memory-safe, no OpenSSL).
- **DNS:** async resolver with DoH (DNS-over-HTTPS) support for privacy.
- **Cache:** HTTP cache honoring cache-control; disk + memory tiers.
- **Cookies & storage jars:** partitioned per site and per **space/profile** (privacy).
- **Content blocking:** request-level filter engine (filter-list matching) sits in
  the network service so blocked requests never leave the machine.
- **Security:** HTTPS-first upgrade, HSTS, cert validation, mixed-content blocking.

The network service is the *only* process with raw socket access; renderers ask it
for resources by capability-scoped IPC.

---

## 6. Storage service (`zero-store`)

- Local, **encrypted-at-rest** stores for: history, bookmarks, passwords (OS keychain
  integration for the master key), cookies, cache, site settings, sessions.
- Embedded DB (e.g. SQLite via `rusqlite`, or a Rust-native KV like `redb`) — a stored
  DB is not "embedding a browser engine," so it's within the from-scratch boundary.
- **Sync (Should):** optional, end-to-end encrypted; keys never leave the device;
  servers self-hostable / India-hosted.

---

## 7. AI service (`zero-ai`)

A dedicated, sandboxed process so AI features can't directly touch page memory or disk.

- **Interfaces:** command-bar intents, "chat with page," summarize, agentic actions.
- **Context extraction:** the renderer produces a sanitized, structured snapshot of
  the current page (text + semantic structure), passed to the AI service — never raw DOM with secrets.
- **Model routing:**
  - *On-device model* (default where hardware allows) — private, no network.
  - *Cloud model* — only on explicit, per-action consent; requests India-routable; no
    content retained without opt-in.
- **Agentic actions:** the AI proposes actions; a **confirmation + capability gate**
  in the browser process executes them. The AI never gets ambient authority.
- **Privacy invariant:** no page content leaves the device unless the user consented for that action.

---

## 8. UI shell & compositing (`zero-shell`)

- **Native GUI** for browser chrome (window, vertical tab rail, toolbar, command bar,
  split view, spaces). Rendered on the GPU for buttery animation.
- **Toolkit decision (open):** options — (a) `wgpu` + a Rust immediate/retained-mode
  UI layer we control (max control, matches "from scratch," most work); (b) a mature
  Rust GUI toolkit for the chrome (faster to build). *Recommendation:* build the chrome
  on our own `wgpu`-based renderer so the shell and web content share one compositor
  and one design system — decision to confirm in Roadmap Phase 0.
- **Windowing / input:** cross-platform window + event handling (`winit`-class) for
  Win/macOS/Linux; native title-bar integration per platform.
- **Design system** is code-level tokens shared with the UI-UX spec (see `02-UI-UX-SPEC.md`).

---

## 9. Rust crate / workspace map

A Cargo workspace. Names indicative.

**What exists today** is two crates, not twenty: `zero-engine` (HTML, CSS, style,
layout, paint, JS, in one crate) and `zero-shell` (window, chrome, tabs, network,
storage, AI, i18n). The split below is the shape this grows *into* as the pieces
earn their own boundaries — a module that has never had a second consumer does
not need to be a crate. The one boundary already real is the important one: the
engine knows nothing about windows, tabs or files, and the shell is one embedder
among possible others.

```
zero/
├── crates/
│   ├── zero-shell        # UI chrome, vertical tabs, spaces, split view, command bar
│   ├── zero-ui           # design-system widgets, wgpu-based UI renderer
│   ├── zero-browser      # main process: orchestration, IPC broker, tab/session mgr
│   ├── zero-ipc          # typed IPC messages + transport
│   ├── zero-html         # HTML tokenizer + DOM
│   ├── zero-css          # CSS parser + stylesheet model
│   ├── zero-style        # cascade, matching, computed style, invalidation
│   ├── zero-layout       # box tree, block/inline/flex/grid, text + Indic shaping
│   ├── zero-paint        # display-list generation
│   ├── zero-compositor   # wgpu layerization, raster, present (GPU service)
│   ├── zero-js           # ECMAScript engine (interpreter → VM → JIT)
│   ├── zero-dom-bindings # JS ↔ DOM / Web API bridge
│   ├── zero-net          # network service: HTTP, TLS, DNS, cache, filtering
│   ├── zero-store        # storage service: encrypted local stores, sync
│   ├── zero-ai           # AI service: context, model routing, agentic gate
│   ├── zero-sandbox      # per-OS sandbox setup (AppContainer/Seatbelt/seccomp)
│   ├── zero-i18n         # localization, IME, Indic input, RTL/complex scripts
│   ├── zero-updater      # signed auto-update
│   └── zero-compat       # (flagged) embedded-engine bridge, Phase-gated
├── docs/
└── Cargo.toml            # workspace
```

**Leveraged crates (not "embedding an engine"):** `tokio`, `wgpu`, `winit`, `rustls`,
`hyper`, `rusqlite`/`redb`, `rustybuzz`, `ttf-parser`, `serde`. These are
infrastructure libraries, consistent with building the *browser* from scratch while
not reinventing GPU APIs, async runtimes, or TLS math.

---

## 10. Web-platform support, phased

The engine's supported subset grows per phase. Each phase defines a **conformance
target** (a fixed test corpus that must render pixel-correct).

| Capability | P1 | P2 | P3 | P4+ |
|-----------|----|----|----|-----|
| HTML parsing (full WHATWG) | ✅ | ✅ | ✅ | ✅ |
| CSS: block, inline, box model, colors, backgrounds | ✅ | ✅ | ✅ | ✅ |
| CSS: flexbox | – | ✅ | ✅ | ✅ |
| CSS: grid | – | – | ✅ | ✅ |
| CSS: transforms, transitions, animations | – | ◑ | ✅ | ✅ |
| Text: Latin + Indic shaping | ✅ | ✅ | ✅ | ✅ |
| Images (PNG/JPEG/WebP/SVG-basic) | ◑ | ✅ | ✅ | ✅ |
| JS: interpreter (ES subset) | ✅ | ✅ | ✅ | ✅ |
| JS: bytecode VM + IC | – | ✅ | ✅ | ✅ |
| JS: baseline JIT | – | – | ◑ | ✅ |
| Web APIs: fetch, DOM, events, storage | ◑ | ✅ | ✅ | ✅ |
| Media: `<video>`/`<audio>` | – | – | ◑ | ✅ |
| Cross-site iframe process isolation | – | – | ✅ | ✅ |
| Compat bridge (fallback) | ✅ | ✅ | ◑ | – |

`✅` supported · `◑` partial · `–` not yet. Corpus and exact property lists tracked per-phase in the Roadmap.

---

## 11. The compatibility bridge (`zero-compat`)

**Purpose:** so early users can browse the *whole* web while our engine matures,
without pretending the from-scratch engine already handles everything.

- Behind a feature flag / opt-in. A tab either renders on **Zero engine** or, for
  sites outside the supported subset, falls back to an **embedded engine**
  (candidate: a system WebView, or an embedded engine component).
- The tab UI clearly indicates which engine rendered the page (trust + transparency).
- As the Zero engine expands per phase, the bridge shrinks and is eventually removed.
- **This is a strategic decision (PRD open question #4):** ship publicly for
  usability, or dev-only to keep the "pure Zero engine" story clean. Recommendation:
  ship as an explicit, labeled opt-in during beta; remove by 1.0-for-general-web.

`ponytail:` the bridge is deliberate scaffolding with a defined removal path — it
prevents the far larger waste of a browser nobody can use for real sites in year one.

---

## 12. Performance budget (reference hardware: 4-core, 8GB, iGPU)

| Budget | Target | Enforcement |
|--------|--------|-------------|
| Cold start | ≤ 1500ms | startup profiling in CI |
| New-tab create | ≤ 100ms | perf test |
| Frame budget (UI) | ≤ 16.6ms (60fps) | compositor instrumentation |
| Input latency (key→paint) | ≤ 50ms | perf test |
| Idle RAM / background tab | target set after P2 baseline; must beat Chromium on same pages | memory harness |
| Renderer process cap | discard/sleep under memory pressure | storage+browser coordination |

Perf is tracked in CI against the conformance corpus; regressions block merge.

---

## 13. Security model (summary; full doc in `04-SECURITY-PRIVACY.md`)

- **Sandbox every renderer + service**; browser process is the only trusted one.
- **Site isolation** keyed by eTLD+1 (phased in).
- **Capability IPC** — no ambient authority; renderers request narrowly scoped resources.
- **Rust safety** eliminates whole exploit classes (UAF, buffer overflow) in safe code;
  `unsafe` blocks are inventoried and reviewed.
- **JIT hardening** (when introduced): W^X, guard pages, control-flow integrity.
- **Update security:** signed, verified updates over TLS; rollback protection.

---

## 14. Testing & CI strategy

- **Unit tests** per crate.
- **Conformance corpus** per phase — reference pages rendered to pixels, diffed against
  golden images (screenshot/ref tests). Web Platform Tests (WPT) subset as the engine grows.
- **JS conformance** — Test262 subset, expanding per phase.
- **Fuzzing** — HTML/CSS/JS parsers and the network stack are fuzzed continuously
  (parsers on untrusted input = top attack surface).
- **Perf CI** — budgets in §12 enforced.
- **Cross-platform CI** — Windows, macOS, Linux runners.

---

## 15. Key architectural risks

| Risk | Impact | Mitigation |
|------|--------|-----------|
| From-scratch engine timeline | High | Phasing + compat bridge; ship value early via shell/AI |
| JS engine correctness/perf | High | Interpreter-first, Test262, defer JIT |
| Cross-platform sandbox parity | Med | Per-OS `zero-sandbox`; lean on OS primitives |
| Indic text shaping correctness | Med | Use proven shaping (`rustybuzz`), extensive corpus |
| GPU driver variance (iGPU/old drivers) | Med | `wgpu` backends (Vulkan/Metal/DX12/GL) + software fallback |
| Team scale for a browser | High | Open-source contribution model, modular crates |

---

## 16. Decisions to confirm (feed back into PRD open questions)

1. UI shell renderer: own `wgpu` UI vs mature Rust GUI toolkit (§8).
2. JS GC strategy timeline (mark-sweep → generational).
3. Compat-bridge distribution policy (§11 / PRD Q4).
4. Embedded DB: `rusqlite` vs `redb`.
5. HTTP stack: adopt `hyper` vs own HTTP/2 on `tokio`.
