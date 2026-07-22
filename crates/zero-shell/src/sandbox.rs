//! What this process is allowed to do, narrowed at startup.
//!
//! This is the half of the sandbox that does not need a second process: the
//! mitigations a process can impose on *itself*, before it has parsed a byte of
//! anything a website sent. They are cheap, they are permanent for the life of
//! the process, and they close the techniques an exploit would reach for after
//! a memory-safety bug — loading its own code, injecting a DLL, reusing a
//! closed handle.
//!
//! **What this is not.** It is not site isolation, and it is not a renderer
//! jail: page content is still parsed and run in the same process as your
//! profile. Those need the process split that `docs/03-ROADMAP.md` describes,
//! and no amount of self-imposed policy substitutes for it. What follows makes
//! a bug harder to turn into code execution; it does not make one harmless.
//!
//! ponytail: Windows only. macOS Seatbelt and Linux seccomp-bpf both need a
//! policy written per syscall surface and a crate to install it, and a policy
//! that has never run on the machine writing it is a policy that will be wrong.

/// Apply every mitigation this platform offers. Failures are ignored on
/// purpose: an older Windows without a policy should still start the browser.
pub fn harden() {
    backend::harden();
}

/// A one-line description for the settings page, so the limits are visible
/// rather than implied.
pub fn describe() -> &'static str {
    backend::DESCRIPTION
}

#[cfg(windows)]
mod backend {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, SetProcessMitigationPolicy, ProcessDynamicCodePolicy,
        ProcessExtensionPointDisablePolicy, ProcessStrictHandleCheckPolicy,
        PROCESS_MITIGATION_POLICY,
    };

    pub const DESCRIPTION: &str =
        "Dynamic code, injected extensions and stale handles are blocked in this process";

    /// Each policy is a struct of bitflags whose first field is a `u32`, so one
    /// helper sets all of them.
    fn set(policy: PROCESS_MITIGATION_POLICY, flags: u32) -> bool {
        // SAFETY: every one of these policies is a 4-byte flags word; we pass
        // exactly that, and the call only reads it.
        unsafe {
            SetProcessMitigationPolicy(
                policy,
                &flags as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>(),
            ) != 0
        }
    }

    pub fn harden() {
        let _ = GetCurrentProcess; // documents the target: this process, always

        // Prohibit dynamic code: no page this process maps can be made
        // executable later. A JIT would have to opt out of this, which is a
        // decision worth being forced to make deliberately (see the roadmap's
        // note on `zero-js`).
        set(ProcessDynamicCodePolicy, 0x1);
        // No AppInit DLLs, no window hooks, no legacy IME injection: three
        // long-standing ways for other software to run code in this process.
        set(ProcessExtensionPointDisablePolicy, 0x1);
        // Using a closed or wrong-type handle raises an exception instead of
        // quietly acting on whatever now occupies that slot.
        set(ProcessStrictHandleCheckPolicy, 0x1 | 0x2);
    }
}

#[cfg(not(windows))]
mod backend {
    pub const DESCRIPTION: &str = "No process mitigations on this platform yet";

    pub fn harden() {}
}
