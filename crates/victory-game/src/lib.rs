//! Everything specific to the title: its container formats, build
//! identities, and the hooks we place in its code (located by per-build
//! offsets or byte-pattern signatures). What may and may not be stored here
//! is defined in docs/asset-policy.md.

pub mod sgd5;

/// A specific build of the title executable we know how to run.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    /// Our own label, e.g. "1.0.0 retail".
    pub label: &'static str,
    /// BLAKE3 of the original executable, hex.
    pub blake3: &'static str,
}

/// The Series X|S 1.0.2.1 executable (the owner's runs 1-12).
pub const BUILD_1_0_2_1: &str = "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93";

/// Builds we know.
pub const KNOWN_BUILDS: &[KnownBuild] = &[KnownBuild {
    label: "Series X|S 1.0.2.1",
    blake3: BUILD_1_0_2_1,
}];

/// Where the title's hang watchdog lives in one build (research checklist
/// R42). A thread polls once a second; when the main thread's heartbeat
/// (a QueryPerformanceCounter value the main loop stores) has not changed
/// for 30 polls, or one of 64 timed scopes is over 300 s old, it calls a
/// handler that writes to address 0, and writes to address 0 itself if
/// the handler returns. The loop has two copies (which one runs depends
/// on the title's configuration), each with its call of the handler.
#[derive(Debug, Clone, Copy)]
pub struct Watchdog {
    /// The heartbeat's writer: `mov [rip + disp32], rax` (7 bytes), whose
    /// target is the heartbeat.
    pub heartbeat_store: usize,
    /// In each loop: `mov rax, [rip + disp32]; test rax, rax; je +2;
    /// call rax` (14 bytes), the handler's call, followed by the fault.
    pub handler_calls: [(usize, [u8; 14], usize); 2],
    /// The handler's own fault (11 bytes).
    pub handler_fault: (usize, [u8; 11]),
}

/// The watchdog of a build, by its BLAKE3.
pub fn watchdog(blake3_hex: &str) -> Option<Watchdog> {
    if !blake3_hex.eq_ignore_ascii_case(BUILD_1_0_2_1) {
        return None;
    }
    Some(Watchdog {
        heartbeat_store: 0x22_159F0 + 0x16,
        handler_calls: [
            // The heartbeat loop: back to its next poll at +0x2215c41.
            (
                0x22_15C27,
                [0x48, 0x8B, 0x05, 0x1A, 0x07, 0xFA, 0x04, 0x48, 0x85, 0xC0, 0x74, 0x02, 0xFF, 0xD0],
                0x22_15C41,
            ),
            // The other loop: back through +0x2215d86 (`mov eax, ebp`).
            (
                0x22_15D9B,
                [0x48, 0x8B, 0x05, 0xA6, 0x05, 0xFA, 0x04, 0x48, 0x85, 0xC0, 0x74, 0x02, 0xFF, 0xD0],
                0x22_15D86,
            ),
        ],
        handler_fault: (0x23_CF683, [0xC7, 0x04, 0x25, 0, 0, 0, 0, 0x21, 0xD4, 0x8B, 0x4D]),
    })
}

/// Looks up a build by the BLAKE3 hash of its executable.
pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    KNOWN_BUILDS
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_known_build_has_its_watchdog_and_others_none() {
        let w = watchdog(BUILD_1_0_2_1).expect("1.0.2.1 is known");
        for (at, expect, back) in w.handler_calls {
            // Our jump back is a short one from after the 14 bytes.
            assert!(i8::try_from(back as i64 - (at + 14) as i64).is_ok());
            assert_eq!(expect[..3], [0x48, 0x8B, 0x05]);
            assert_eq!(expect[12..], [0xFF, 0xD0]);
        }
        assert!(watchdog("0123").is_none());
        assert_eq!(identify(BUILD_1_0_2_1).map(|b| b.label), Some("Series X|S 1.0.2.1"));
    }
}
