# Zero Browser — Security & Privacy Specification

**Version:** 0.1 (Draft)
**Last updated:** 2026-07-21

Security and privacy are Zero's product, not a checkbox. This doc defines the threat
model, the sandbox/isolation model, data handling, and the privacy invariants that the
architecture must uphold.

---

## 1. Security principles

1. **Assume every web page is hostile.** Rendering untrusted content is the core threat.
2. **Least privilege / no ambient authority.** Every process gets only the capabilities it needs.
3. **Contain, don't just prevent.** Even if a renderer is exploited, it must not reach
   other sites' data or the OS.
4. **Memory safety by default.** Rust removes whole exploit classes in safe code; `unsafe`
   is inventoried, minimized, and reviewed.
5. **Transparent & auditable.** Open source; security decisions documented; audits welcomed.

---

## 2. Threat model

| Adversary | Goal | Zero's defense |
|-----------|------|----------------|
| Malicious website | Escape renderer, run code on host | Sandbox + Rust safety + site isolation |
| Malicious website | Read another site's cookies/data | Site isolation + partitioned storage |
| Malicious website | Fingerprint / track the user | Anti-fingerprinting, tracker blocking, partitioning |
| Network attacker (MITM) | Intercept/alter traffic | TLS (`rustls`), HTTPS-first, HSTS, cert validation |
| Malicious network/DNS | Surveil browsing | DNS-over-HTTPS, encrypted SNI where available |
| Compromised update channel | Push malicious update | Signed, verified updates; rollback protection |
| Local attacker (device access) | Read stored data | Encryption at rest, OS keychain for keys |
| Malicious/abusive AI action | Take unwanted actions | Capability gate + explicit user confirmation |
| Supply chain (deps) | Inject via a crate | Dependency review, `cargo audit`, minimal deps, pinning |

**Out of scope (v1):** nation-state 0-day against the OS kernel; physical hardware attacks;
protecting against a fully compromised host OS.

---

## 3. Process & sandbox model

(See Architecture §2 for the diagram.) Renderers and services are sandboxed; the browser
process is the only trusted one.

### 3.1 Per-OS sandbox (`zero-sandbox`)
| OS | Primitives |
|----|-----------|
| **Windows** | AppContainer, restricted tokens, job objects, Win32k lockdown, low integrity |
| **macOS** | App Sandbox / Seatbelt profiles, entitlements minimization |
| **Linux** | seccomp-bpf syscall filtering, user namespaces, `pivot_root`/namespaces, no-new-privs |

Renderers have **no** direct filesystem, network socket, or GPU device access. All such
operations are brokered over capability-scoped IPC to the network / storage / GPU services.

### 3.2 Site isolation
- Renderer processes keyed by site (**eTLD+1**). Phased: per-tab in P1 → per-site with
  out-of-process iframes by P3.
- Cross-origin data (cookies, storage, cache) is **partitioned** by top-level site so a
  page can't reach another site's state even in the same process class.

### 3.3 IPC security
- Typed messages; each renderer's channel is scoped to its own tab/site capabilities.
- The browser process validates every request; a renderer cannot request another site's resources.
- No renderer can enumerate or address another renderer.

### 3.4 `unsafe` policy
- Every `unsafe` block carries a comment justifying soundness and is listed in an audit registry.
- CI denies new `unsafe` outside allow-listed crates (e.g. FFI/GPU boundaries) without review.

---

## 4. Web-content security (must implement as the engine grows)

- **Same-origin policy** enforced in `zero-dom-bindings` / `zero-net`.
- **CORS** for cross-origin fetch/resources.
- **Content-Security-Policy (CSP)** parsing + enforcement.
- **Mixed-content** blocking; **HTTPS-first** with upgrade.
- **HSTS**, secure-cookie / `SameSite` handling, `HttpOnly` respected.
- **Subresource integrity (SRI)** where specified.
- **X-Frame-Options / frame-ancestors** clickjacking protection.
- **JIT hardening** (when introduced): W^X memory, guard pages, control-flow integrity,
  structure guards — the JIT is a prime exploit surface and treated as such.

---

## 5. Privacy model

### 5.1 Invariants (non-negotiable)
- **No telemetry without explicit opt-in.** Default = nothing leaves the device.
- **No page content leaves the device unless the user consented for that specific action**
  (applies especially to AI cloud calls).
- **No ad networks, no data sales, ever.** Business model is OSS + optional paid cloud services.
- **User owns their data and keys.** Sync is end-to-end encrypted; keys never leave the device.

### 5.2 Tracking protection
- Built-in request-level content blocker (EasyList/EasyPrivacy-class + India-relevant lists),
  running in the network service so blocked requests never leave the machine. On by default.
- **Storage partitioning** (state partitioned by top-level site) to break cross-site tracking.
- **Anti-fingerprinting:** reduce/normalize high-entropy surfaces (canvas, fonts, UA,
  screen metrics); "resist fingerprinting" mode.
- **DNS-over-HTTPS** to prevent DNS-based surveillance.
- Per-site permissions default-deny (camera, mic, location, notifications).

### 5.3 Data at rest
- History, bookmarks, passwords, cookies, cache, sessions stored **locally, encrypted**.
- Master key protected via OS keychain (Windows DPAPI/Credential Manager, macOS Keychain,
  Linux Secret Service / kwallet).
- Passwords: strong KDF, encrypted vault; optional master password.

### 5.4 Data sovereignty (India positioning)
- Any optional cloud services (sync, premium AI) are **India-hostable / self-hostable**.
- Transparency report cadence; clear docs on what (if anything) is processed and where.
- **Data passport** UI: user sees exactly what's stored, where, and can export/delete.

### 5.5 AI privacy
- **On-device model is the default** where hardware permits — zero network for AI.
- Cloud AI is **explicit, per-action consent**; requests India-routable; **no retention**
  without separate opt-in.
- Page context sent to AI is **sanitized** (structured text, secrets/passwords stripped),
  never raw DOM with credentials.
- Agentic actions require an explicit confirmation gate; the AI never has ambient authority.

---

## 6. Update & supply-chain security

- Updates are **signed**; signature + version verified before apply; **rollback protection**
  prevents downgrade attacks. Delivered over TLS.
- Dependencies: minimal set, reviewed, `cargo audit` in CI, version pinning, reproducible
  builds as a goal.
- Release artifacts signed; checksums published.

---

## 7. Testing & assurance

- **Fuzzing** of all untrusted-input parsers (HTML, CSS, JS, image decoders, network) —
  continuous, since parsers are the top attack surface.
- **Security regression tests** for SOP/CORS/CSP/mixed-content.
- **Sandbox escape tests** per OS.
- **`cargo audit` + dependency review** in CI.
- **External security audit** before 1.0; **bug-bounty** program at/after 1.0.
- **Privacy tests:** assert no unexpected network egress in default config (a test that
  fails if the browser "phones home" without consent).

---

## 8. Responsible disclosure

- Public `SECURITY.md` with a disclosure address and policy.
- Coordinated disclosure window; credit researchers; publish advisories.
- (Post-1.0) funded bug-bounty tiers.

---

## 9. Open security decisions

1. Software-rendering fallback path — keep it as sandbox-friendly as GPU path.
2. Password store: build vault vs integrate a proven Rust crypto vault design.
3. Extension API sandboxing model (P3) — how much power to expose safely.
4. Reproducible-build tooling and signing infrastructure ownership.
