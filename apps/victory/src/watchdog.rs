//! The title's hang watchdog (research checklist R42): watched, and with
//! `VICTORY_NO_WATCHDOG=1` kept from ending the run.
//!
//! The title ends the process on purpose when its main thread's heartbeat
//! has not moved for 30 one-second polls (a write to address 0). Run 1 ended
//! that way while ~950 pipelines were made in one frame, and the base's
//! runs with the ray-tracing structure builder on ended that way 30-40 s
//! after the builder started: what the main thread was waiting for then is
//! the question to answer before the builder can be on.
//!
//! - **Watched** (on unless `VICTORY_STALL_WATCH=0`): a thread of ours reads
//!   the heartbeat once a second and, once it has not moved for 10 s,
//!   says so every 10 s with what our side was doing (the submitting
//!   thread, the trail of recent work, fences the title waits for, the
//!   builder), and when it moves again.
//! - **Kept from ending the run** (`VICTORY_NO_WATCHDOG=1`; off by
//!   default): both of the watchdog's calls of its handler call ours
//!   instead, which logs the same, and the loop goes on to its next poll
//!   (with the heartbeat's count left at 0, so it fires once a stall). The
//!   handler's own fault becomes a no-op. A real deadlock stays one: this
//!   is for finding out what a long wait is, not a fix.
//!
//! Both only for a build whose sites are known (`victory_game::watchdog`)
//! and whose bytes there are exactly the expected ones.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

/// When our handler was last called, for its line.
static STARTED: OnceLock<Instant> = OnceLock::new();

/// Sets the watchdog up for the image at `base` with BLAKE3 `hash`, before
/// any title code runs.
pub fn arm(base: usize, hash: &str) {
    let Some(w) = victory_game::watchdog(hash) else {
        tracing::info!("the title's watchdog: not known for this build; not watched");
        return;
    };
    STARTED.get_or_init(Instant::now);
    let disarm = std::env::var_os("VICTORY_NO_WATCHDOG").is_some_and(|v| v == "1");
    if disarm {
        let mut applied = 0;
        let ours = handler as extern "system" fn() as usize as u64;
        for &(at, expect, back) in &w.handler_calls {
            // mov rax, imm64 (ours); call rax; jmp back (rel8 from the
            // instruction after the 14 bytes).
            let after = at + 14;
            let Some(rel) = i8::try_from(back as i64 - after as i64).ok() else {
                continue;
            };
            let mut new = [0u8; 14];
            new[0] = 0x48;
            new[1] = 0xB8;
            new[2..10].copy_from_slice(&ours.to_le_bytes());
            new[10] = 0xFF;
            new[11] = 0xD0;
            new[12] = 0xEB;
            new[13] = rel as u8;
            // SAFETY: the image is mapped and no title code runs yet; the
            // bytes are checked first.
            applied += usize::from(unsafe { patch(base + at, &expect, &new) });
        }
        // An eleven-byte no-op over the handler's own fault.
        let nop = [0x66, 0x66, 0x2E, 0x0F, 0x1F, 0x84, 0, 0, 0, 0, 0];
        // SAFETY: as above.
        applied += usize::from(unsafe { patch(base + w.handler_fault.0, &w.handler_fault.1, &nop) });
        tracing::warn!(
            applied,
            of = 3,
            "the title's watchdog is kept from ending the run (VICTORY_NO_WATCHDOG=1): a stall of 30 s is logged and play goes on"
        );
    }
    if std::env::var_os("VICTORY_STALL_WATCH").is_some_and(|v| v == "0") {
        return;
    }
    // The heartbeat: where the writer stores (`mov [rip + disp32], rax`).
    let store = base + w.heartbeat_store;
    // SAFETY: inside the mapped image's code.
    let bytes = unsafe { std::slice::from_raw_parts(store as *const u8, 7) };
    if bytes[..3] != [0x48, 0x89, 0x05] {
        tracing::warn!("the title's watchdog: the heartbeat's writer is not where it was expected; not watched");
        return;
    }
    let disp = i32::from_le_bytes([bytes[3], bytes[4], bytes[5], bytes[6]]);
    let heartbeat = (store as i64 + 7 + i64::from(disp)) as usize;
    // The timed scopes' count is the dword before it (R42).
    let scopes = heartbeat - 4;
    let spawned = std::thread::Builder::new()
        .name("victory-stall-watch".into())
        .spawn(move || watch(heartbeat, scopes));
    if spawned.is_ok() {
        tracing::info!("the title's main thread is watched for stalls (VICTORY_STALL_WATCH=0: not)");
    }
}

/// Reads the heartbeat once a second; says when it stands still.
fn watch(heartbeat: usize, scopes: usize) {
    let read = || {
        // SAFETY: a field of the title's data section, written by its main
        // thread; a torn read only delays the line by a second.
        unsafe { std::ptr::read_volatile(heartbeat as *const u64) }
    };
    let (mut last, mut since) = (read(), Instant::now());
    let mut said = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        let now = read();
        if now != last {
            if said > 0 {
                tracing::warn!(
                    seconds = since.elapsed().as_secs(),
                    "the title's main thread moves again after a stall"
                );
            }
            (last, since, said) = (now, Instant::now(), 0);
            continue;
        }
        // Nothing written yet: the main loop has not started.
        if now == 0 {
            continue;
        }
        let stood = since.elapsed().as_secs();
        if stood >= 10 && stood / 10 > said {
            said = stood / 10;
            // SAFETY: as above.
            let open = unsafe { std::ptr::read_volatile(scopes as *const u32) };
            tracing::warn!(
                seconds = stood,
                timed_scopes = open,
                "the title's main thread has not moved (its watchdog ends the run at 30 s unless VICTORY_NO_WATCHDOG=1): {}",
                victory_hle::stall_report()
            );
        }
    }
}

/// Called by the title's watchdog thread where it would call its handler.
extern "system" fn handler() {
    let at = STARTED.get().map_or(0, |s| s.elapsed().as_secs());
    tracing::warn!(
        seconds_into_run = at,
        "the title's watchdog fired and was kept from ending the run (VICTORY_NO_WATCHDOG=1): {}",
        victory_hle::stall_report()
    );
}

/// Writes `new` over `expect` at `at` if the bytes there are `expect`.
///
/// # Safety
/// `at` is inside the mapped title image's code and no title code runs.
unsafe fn patch(at: usize, expect: &[u8], new: &[u8]) -> bool {
    // SAFETY: forwarded.
    let now = unsafe { std::slice::from_raw_parts(at as *const u8, expect.len()) };
    if now != expect || new.len() != expect.len() {
        tracing::warn!(at = format_args!("{at:#x}"), "the title's watchdog: bytes not the expected ones; left as they are");
        return false;
    }
    let mut old = 0;
    // SAFETY: those bytes only; the protection is put back below.
    if unsafe { VirtualProtect(at as _, new.len(), PAGE_EXECUTE_READWRITE, &mut old) } == 0 {
        return false;
    }
    // SAFETY: writable now; checked above.
    unsafe {
        std::ptr::copy_nonoverlapping(new.as_ptr(), at as *mut u8, new.len());
        VirtualProtect(at as _, new.len(), old, &mut old);
        FlushInstructionCache(GetCurrentProcess(), at as _, new.len());
    }
    true
}
