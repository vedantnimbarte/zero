# Zero Browser — Product Requirements Document (PRD)

**Version:** 0.1 (Draft)
**Owner:** Vedant Nimbarte
**Last updated:** 2026-07-21
**Status:** Draft for review

---

## 1. Summary

Zero is an open-source, privacy-first, AI-native desktop web browser built
entirely from scratch in Rust — including its own rendering engine, JavaScript
engine, and networking stack. It targets Windows, macOS, and Linux at launch.

Zero's product surface is minimal and spacious (design language inspired by Arc,
Dia, and Comet) with **vertical tabs** as the primary navigation model. Its
differentiation rests on four pillars: **privacy & data sovereignty**,
**performance & minimalism**, **AI-native browsing**, and **India-localization**.

Positioning line: *"India's own browser — private by default, fast by design,
intelligent by nature."*

---

## 2. Problem statement

1. **Dependence.** Nearly every browser Indians use is foreign-owned and
   Chromium/WebKit-based (Chrome, Edge, Brave, Arc, Comet). There is no
   sovereign, from-the-ground-up Indian browser.
2. **Privacy erosion.** Mainstream browsers monetize attention and data;
   trackers are pervasive; users have little real control.
3. **Bloat & clutter.** Traditional browser chrome is dense and horizontal-tab
   heavy; tab overload is a daily pain.
4. **AI is bolted on.** Where AI exists, it's a sidebar afterthought, not woven
   into how you navigate and act on the web.
5. **Weak localization.** Indian-language UX, local search, and India-specific
   integrations (UPI, DigiLocker, regional content) are underserved.

---

## 3. Vision & strategy

**Vision:** Every Indian on the web through a browser they can trust — that
respects their privacy, runs fast on modest hardware, understands their
languages, and helps them get things done with AI.

**Strategy:** Win a passionate early-adopter base (privacy-conscious users,
developers, students, designers) with a beautiful, minimal, AI-native shell —
while transparently building out a genuine Rust engine in phases. Use the
open-source community as both contributor pool and trust signal.

**Moat over time:** (a) a sovereign, auditable, Rust-safe engine; (b) India-first
localization and integrations that global players deprioritize; (c) an AI layer
that is private and on-device-first.

---

## 4. Goals & non-goals

### 4.1 Goals (v1)
- Ship a stable, beautiful desktop browser on Windows/macOS/Linux.
- Vertical tabs, spaces/profiles, split view, command bar.
- Privacy by default: tracker/ad blocking, no non-consensual telemetry, local data.
- AI-native: page chat, summarize, agentic actions, smart command bar.
- India-localization: multi-language UI, Indian-language search, local integrations.
- Own rendering + JS engine handling a defined web subset, with a compat fallback.

### 4.2 Non-goals (v1)
- Full parity with Chromium web compatibility (explicitly a multi-phase journey).
- Mobile (Android/iOS) apps — later phase.
- Browser extension marketplace at parity with Chrome Web Store (limited API in v1).
- Enterprise device-management/MDM tooling.
- Ad network or data-monetization business model (contradicts positioning).

---

## 5. Target users & personas

| Persona | Description | Primary need |
|---------|-------------|--------------|
| **Priya, the privacy-conscious professional** | 28, urban, uses many web apps | Trust, no tracking, clean UX |
| **Arjun, the developer/enthusiast** | 24, tinkerer, OSS contributor | Rust internals, hackability, speed |
| **Meera, the student** | 20, budget laptop, studies in Hindi/regional lang | Low RAM, localized UI, AI study help |
| **Rohan, the power multitasker** | 32, 40+ tabs open daily | Vertical tabs, spaces, split view |
| **Community contributor** | Global/Indian OSS dev | Clean architecture, good docs |

---

## 6. Product pillars & requirements

### Pillar A — Privacy & data sovereignty
- **P-A1 (Must):** Built-in tracker + ad blocker on by default (EasyList/EasyPrivacy-class + Indian lists).
- **P-A2 (Must):** No telemetry collected without explicit opt-in; when opted in, anonymized and India-hosted.
- **P-A3 (Must):** All user data (history, bookmarks, passwords, cache) stored locally, encrypted at rest.
- **P-A4 (Should):** Optional encrypted sync with user-held keys; sync servers hostable in India / self-host.
- **P-A5 (Should):** Per-site permission model, fingerprinting resistance, HTTPS-first.
- **P-A6 (Could):** "Data passport" — a plain-language dashboard of exactly what's stored and where.

### Pillar B — Performance & minimalism
- **P-B1 (Must):** Cold start ≤ 1.5s; new-tab open ≤ 100ms on reference hardware (see §10).
- **P-B2 (Must):** GPU-accelerated compositing; 60fps UI animations; 120fps where display supports.
- **P-B3 (Must):** Idle RAM per background tab meaningfully below Chromium baseline (target set in Architecture).
- **P-B4 (Must):** Minimal, spacious chrome; hide-able toolbar; content-first layout.
- **P-B5 (Should):** Background tab sleeping/discarding; memory pressure handling.

### Pillar C — AI-native browsing
- **C-1 (Must):** AI command bar — natural-language navigation, search, and actions from one input.
- **C-2 (Must):** "Chat with this page" — summarize, ask questions, extract, using current tab context.
- **C-3 (Should):** Cross-tab / space context ("compare these 3 tabs").
- **C-4 (Should):** Agentic actions (fill forms, multi-step tasks) with explicit user confirmation.
- **C-5 (Must):** Privacy-preserving AI: on-device model option; cloud calls are explicit, consented, and India-routable.
- **C-6 (Could):** Writing/coding assist inline in text fields.

### Pillar D — India-localization
- **D-1 (Must):** UI localized in English + Hindi at launch; framework for ≥8 more Indian languages.
- **D-2 (Should):** Indian-language search & AI (query and answer in regional languages).
- **D-3 (Should):** Local integrations surface: UPI payment awareness, DigiLocker, government-services quick links.
- **D-4 (Could):** Indian content discovery on new-tab (regional news, opt-in).
- **D-5 (Must):** IME/input support for Indic scripts; correct complex-script text shaping in-engine.

### Core browser (table stakes)
- **T-1:** Vertical tabs with drag-reorder, groups, pinning, close/restore.
- **T-2:** Spaces/profiles (separate cookie/storage jars, visual themes).
- **T-3:** Split view (2+ panes side by side; see reference image 1).
- **T-4:** Address/command bar with autocomplete, history, bookmarks.
- **T-5:** New-tab page with quick links, search, top stories (reference image 2).
- **T-6:** Downloads, history, bookmarks managers.
- **T-7:** Find-in-page, zoom, reader mode.
- **T-8:** Session restore, crash recovery.
- **T-9:** Settings/preferences with privacy front-and-center.
- **T-10:** Auto-update (secure, signed).

*(Must / Should / Could = MoSCoW prioritization.)*

---

## 7. Key user flows (v1)

1. **First run:** Welcome → choose language → import from existing browser (optional) → privacy defaults explained → done.
2. **Open & navigate:** ⌘/Ctrl-T → command bar → type query or URL → AI-assisted suggestions → land on page.
3. **Vertical tab management:** New tabs stack vertically in left sidebar → drag to reorder/group → pin → collapse sidebar for full content.
4. **Split view:** Right-click tab → "Add Right Split" → two panes; drag divider to resize.
5. **Chat with page:** Click AI icon / shortcut → sidebar chat scoped to current tab → summarize or ask.
6. **Spaces:** Switch space (bottom-left, reference image 1) → different set of tabs, theme, and profile.
7. **Privacy check:** Shield icon in toolbar → see trackers blocked on this site → toggle protections.

---

## 8. Competitive landscape

| Browser | Engine | Vertical tabs | AI | Privacy stance | Notes |
|---------|--------|---------------|----|----------------|-------|
| Chrome | Chromium | No (native) | Add-on | Weak | Market leader |
| Edge | Chromium | Yes | Copilot | Medium | MS ecosystem |
| Arc | Chromium | Yes | Some | Medium | Design leader |
| Dia | Chromium | Yes | AI-native | Medium | Arc's AI successor |
| Comet (Perplexity) | Chromium | Yes | AI-native | Medium | Agentic |
| Brave | Chromium | Optional | Some | Strong | Privacy + crypto |
| **Zero** | **Own (Rust)** | **Yes** | **AI-native** | **Strong + sovereign** | **India-first, from scratch** |

**Zero's unique intersection:** the only one that is *from-scratch Rust engine +
sovereign/India-localized + AI-native + minimal design*.

---

## 9. Business & GTM (open-source product)

- **Model:** Open-source core (permissive license). Optional paid **cloud services**
  (encrypted sync, premium AI compute) fund development — never ads or data sales.
- **Launch sequencing:** Private alpha (contributors) → public beta (early adopters) → 1.0.
- **Community:** Public roadmap, RFC process, contributor guide, Discord/Matrix.
- **Trust/sovereignty angle:** auditable code, India-hosted services, transparency reports.
- **Distribution:** direct download (signed installers), plus Linux repos / package managers.

---

## 10. Success metrics

**Reference hardware for perf targets:** mid-range laptop, 4-core CPU, 8GB RAM,
integrated GPU (representative Indian student/professional device).

| Metric | v1 target |
|--------|-----------|
| Cold start | ≤ 1.5s |
| New tab open | ≤ 100ms |
| UI frame rate | 60fps sustained |
| Idle RAM / background tab | < Chromium baseline (exact figure in Architecture §perf-budget) |
| Crash-free sessions | ≥ 99.5% |
| Tracker requests blocked | ≥ 90% of known-tracker requests |
| Web-subset compatibility | 100% of the Phase-defined subset renders correctly |
| Retention (early adopters) | D7 ≥ 30%, D30 ≥ 15% (beta target) |

---

## 11. Constraints & assumptions

- Small team + open-source contributors; from-scratch engine is the schedule risk.
- Rust ecosystem crates may be leveraged (e.g. `wgpu`, `tokio`) — "from scratch"
  means *we don't embed a browser engine*, not that we rewrite GPU drivers.
- AI cloud calls require a provider; on-device model constrained by user hardware.
- India-hosting requirements may need cloud region availability planning.

---

## 12. Open questions

1. **Product name & brand.** "Zero" is a working codename. Final name, logo, domain?
2. **License choice.** Apache-2.0 (permissive) vs MPL-2.0 (file-level copyleft, Mozilla-style)?
3. **AI provider(s).** Which cloud model for v1? Which on-device model (size/quality trade-off)?
4. **Compat-bridge policy.** Do we ship the embedded-engine fallback publicly, or dev-only?
5. **Funding.** Bootstrapped, grant (e.g. Indian gov/OSS grants), or backed?
6. **Governance.** BDFL, foundation, or company-steered OSS?
7. **Which 8+ Indian languages** are prioritized after Hindi?

---

## 13. Appendix — reference UI

- **Reference image 1:** Arc-style split view, vertical tabs in left rail, spaces
  at bottom-left, per-tab context menu (Move Left/Right, Add Split). Zero adopts
  this layout language.
- **Reference image 2:** Dark, spacious new-tab page — centered search, social
  quick-launch rail, tiled shortcuts, "Top Stories" with category chips, privacy
  assurances under the search bar. Zero's new-tab page follows this direction.

Detailed visual spec: [`02-UI-UX-SPEC.md`](02-UI-UX-SPEC.md).
