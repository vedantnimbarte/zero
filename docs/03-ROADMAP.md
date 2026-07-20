# Zero Browser — Roadmap & Risk Register

**Version:** 0.1 (Draft)
**Last updated:** 2026-07-21

> This roadmap is deliberately honest about the biggest fact of the project: a
> from-scratch browser engine is a multi-year effort. The plan front-loads
> *shippable value* (shell + AI + a real, if limited, engine) and grows web
> compatibility in phases, with a compat bridge so early users are never stuck.

Durations are **relative effort bands**, not calendar promises — they depend on team
size (a from-scratch engine with a small team runs long; contributors accelerate it).

---

## Phase 0 — Foundation & spike (proof it's real)

**Goal:** stand up the workspace, process model, and a "hello web" render.

- Cargo workspace + crate skeletons (§9 of Architecture).
- Multi-process shell: browser process + one sandboxed renderer + IPC.
- `wgpu` window + minimal UI chrome; render a solid color, then a rectangle, then text.
- `zero-html` tokenizer + DOM for a trivial page; `zero-css` for a handful of properties;
  `zero-layout` block layout; `zero-paint` + `zero-compositor` render **a static HTML page**.
- Networking: fetch a URL over HTTPS (`rustls`) and display its (very simple) HTML.
- **Exit criteria:** a sandboxed renderer displays a hand-picked static HTML page, on all 3 OSes.

**Deliverable:** internal demo. Proves the pipeline end-to-end.

---

## Phase 1 — Usable minimal browser (private alpha)

**Goal:** a real, if limited, browser you can daily-drive for simple sites + our own surfaces.

**Engine**
- HTML parsing to WHATWG conformance (the hard malformed-input cases).
- CSS: full box model, inline/block, colors, backgrounds, basic positioning.
- Text: Latin + **Indic shaping** (`rustybuzz`), font fallback, line breaking.
- Images: PNG/JPEG (partial).
- `zero-js` **interpreter** — ES subset; DOM bindings for basic interactivity.
- **Compat bridge** available (opt-in) for sites outside the subset.

**Shell / product**
- Vertical tabs (reorder, pin, close/undo, groups-lite), collapsible sidebar.
- Command/address bar (navigate, search, history/bookmark suggestions).
- New-tab page (per reference image 2), light + dark themes.
- Downloads, history, bookmarks (basic); session restore; settings (privacy-first).
- Privacy: tracker/ad blocking in network service, HTTPS-first, local encrypted storage.

**AI (v1 core)**
- AI service process; command-bar AI intents.
- "Chat with this page" (summarize/ask) via sanitized page context.
- On-device model option + consented cloud; privacy indicator.

**Localization**
- English + Hindi UI; Indic IME in inputs; ₹/date/number formatting.

**Exit criteria:** contributors can use Zero for a defined set of everyday simple sites +
all Zero surfaces; the rest fall back to the bridge with clear labeling.

**Milestone:** **Private Alpha.**

---

## Phase 2 — Real-world capable (public beta)

**Goal:** most mainstream sites are pleasant on the Zero engine.

**Engine**
- CSS **flexbox**; transforms/transitions (partial); more paint fidelity (shadows, gradients, radii).
- Images: WebP + basic SVG.
- `zero-js` **bytecode VM + inline caches** (real-world speed); more Web APIs
  (`fetch`, events, timers, storage, `localStorage`/`IndexedDB`-lite).
- Perf baseline established; memory harness; background-tab sleeping.

**Product**
- Split view (2 panes) with draggable divider + context menu (reference image 1).
- Spaces & profiles (separate storage jars, per-space accent).
- Encrypted sync (optional, E2E, self-hostable).
- Agentic AI actions with confirmation gate; cross-tab context (Should).
- More Indian languages (target: +4).

**Exit criteria:** a broad site corpus renders correctly on the Zero engine; bridge usage
drops sharply; perf budgets met on reference hardware.

**Milestone:** **Public Beta.**

---

## Phase 3 — Compatibility & hardening (1.0)

**Goal:** the browser is trustworthy and compatible enough for a general launch.

**Engine**
- CSS **grid**; fuller animations; `<video>`/`<audio>` (partial).
- **Site isolation** for cross-site iframes; renderer-per-site.
- `zero-js` **baseline JIT** (hardened); GC to generational if profiling demands.
- WPT subset pass-rate target; Test262 subset target.

**Product & trust**
- Security audit + fuzzing hardening; bug-bounty program.
- Signed auto-update with rollback protection.
- Accessibility audit (WCAG AA) pass.
- Data-passport, transparency report, India-hosted services live.
- Extensions: limited, safe extension API (content blocking, themes) — not full Chrome parity.

**Exit criteria:** stable, secure, compatible with the large majority of real sites;
crash-free ≥ 99.5%; bridge is opt-in/legacy only.

**Milestone:** **1.0 Public Launch — "India's first from-scratch browser."**

---

## Phase 4+ — Scale & differentiate (post-1.0)

- Optimizing JIT; HTTP/3/QUIC; more media/codecs; PWA support.
- Deeper India integrations (UPI awareness, DigiLocker, gov services).
- Mobile (Android first) — a large, separate track.
- Broader extension ecosystem; developer tools.
- Retire the compat bridge for general web use.

---

## Cross-cutting workstreams (run throughout)

- **Testing/CI:** conformance corpus, screenshot diffs, fuzzing, perf CI, 3-OS matrix.
- **Security:** sandbox parity per OS, `unsafe` audits, threat-model reviews.
- **Community:** public roadmap, RFCs, contributor guide, good-first-issues, Discord/Matrix.
- **Design:** hi-fi prototype → design system in `zero-ui` → per-screen implementation.
- **Localization:** grow language coverage each phase.

---

## Risk register

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|-----------|
| R1 | From-scratch engine takes far longer than hoped | High | High | Phasing + compat bridge; ship shell/AI value early; grow contributors |
| R2 | JS engine can't reach usable perf | Med | High | Interpreter→VM→JIT staged; Test262; defer optimizing JIT |
| R3 | Web-compat long tail never "done" | High | Med | Accept phased subset publicly; bridge covers the tail; be transparent |
| R4 | Small team / contributor drop-off | Med | High | Modular crates, docs, OSS governance, grants/funding |
| R5 | Cross-OS sandbox complexity | Med | Med | `zero-sandbox` per OS; lean on OS primitives; security review |
| R6 | Indic text shaping bugs | Med | Med | Proven shaper + large script test corpus |
| R7 | GPU/driver variance breaks rendering | Med | Med | `wgpu` multi-backend + software fallback |
| R8 | AI privacy misstep erodes trust | Low | High | On-device default, explicit consent, no retention, audits |
| R9 | Funding runs out before 1.0 | Med | High | Bootstrap+grants; OSS cloud-services revenue; scope discipline |
| R10 | "India's first" claim challenged | Low | Med | Qualify precisely: first *from-scratch, sovereign, Rust* browser |

---

## What "shippable at each phase" means (anti-vaporware guardrail)

Each phase ends with a **thing a real person can install and use**, not a bigger TODO:

- **P0:** internal demo (renders a static page).
- **P1:** private alpha (daily-drive simple sites + Zero surfaces; bridge for the rest).
- **P2:** public beta (most mainstream sites on our engine).
- **P3:** 1.0 (general web, secure, audited).

If a phase can't hit its exit criteria, we cut scope, not honesty.
