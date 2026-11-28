//! Counting the title's calls into its own DLLs (`VICTORY_TITLE_CALLS`).
//!
//! A diagnostic, off unless asked for: it answers "does the title ever call
//! this DLL, and which of its functions" without touching what the calls
//! do. Each import bound to a watched title DLL gets a stub instead of the
//! DLL's own address:
//!
//! ```text
//! mov r11, <&'static AtomicU64>    ; 49 BB imm64
//! lock inc qword [r11]             ; F0 49 FF 03
//! mov r11, <the DLL's function>    ; 49 BB imm64
//! jmp r11                          ; 41 FF E3
//! ```
//!
//! `r11` is scratch at a call in the x64 ABI (never an argument, never
//! kept for the caller), the stub leaves the stack and every argument
//! register alone, and it jumps rather than calls, so the function sees
//! the title's own return address. Stack walks through the stub see a leaf
//! that has pushed nothing, which is what it is.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use victory_pe::ImportSymbol;

use crate::resolve::{ImportResolver, Provenance, Resolution};

const STUB: usize = 32;

/// The stub's bytes: counts into `counter`, then jumps to `target`.
pub(crate) fn stub_code(counter: u64, target: u64) -> [u8; STUB] {
    let mut code = [0xCC_u8; STUB]; // int3 padding
    code[0..2].copy_from_slice(&[0x49, 0xBB]);
    code[2..10].copy_from_slice(&counter.to_le_bytes());
    code[10..14].copy_from_slice(&[0xF0, 0x49, 0xFF, 0x03]);
    code[14..16].copy_from_slice(&[0x49, 0xBB]);
    code[16..24].copy_from_slice(&target.to_le_bytes());
    code[24..27].copy_from_slice(&[0x41, 0xFF, 0xE3]);
    code
}

struct Counted {
    dll: String,
    symbol: String,
    calls: &'static AtomicU64,
    stub: usize,
}

struct Watch {
    dlls: Vec<String>,
    counted: Vec<Counted>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

fn watch() -> std::sync::MutexGuard<'static, Option<Watch>> {
    WATCH.lock().unwrap_or_else(|e| e.into_inner())
}

/// Counts calls into these title DLLs from imports bound from now on
/// through [`Counting`]. Empty: counts nothing.
pub fn count_calls_into(dlls: &[String]) {
    *watch() = (!dlls.is_empty()).then(|| Watch {
        dlls: dlls.to_vec(),
        counted: Vec::new(),
    });
}

/// Whether [`count_calls_into`] named any DLL.
pub fn counting_calls() -> bool {
    watch().is_some()
}

/// Every counted `dll!symbol` and its calls so far, in binding order.
pub fn counted_calls() -> Vec<(String, u64)> {
    watch().as_ref().map_or_else(Vec::new, |w| {
        w.counted
            .iter()
            .map(|c| {
                let name = format!("{}!{}", c.dll, c.symbol);
                (name, c.calls.load(Ordering::Relaxed))
            })
            .collect()
    })
}

/// Binds as `inner` does, except that imports `inner` finds in a watched
/// title DLL get a counting stub in front of them.
pub struct Counting<'a> {
    pub inner: &'a dyn ImportResolver,
}

impl ImportResolver for Counting<'_> {
    fn resolve(&self, dll: &str, symbol: &ImportSymbol) -> Resolution {
        let found = self.inner.resolve(dll, symbol);
        let Resolution::Found {
            address,
            provenance: Provenance::Title,
        } = found
        else {
            return found;
        };
        let mut guard = watch();
        let Some(watch) = guard.as_mut() else {
            return found;
        };
        if !watch.dlls.iter().any(|w| w.eq_ignore_ascii_case(dll)) {
            return found;
        }
        let symbol = symbol.to_string();
        let known = watch
            .counted
            .iter()
            .find(|c| c.dll.eq_ignore_ascii_case(dll) && c.symbol == symbol);
        if let Some(c) = known {
            return Resolution::Found {
                address: c.stub,
                provenance: Provenance::Title,
            };
        }
        let calls: &'static AtomicU64 = Box::leak(Box::new(AtomicU64::new(0)));
        let Some(stub) = place(&stub_code(calls as *const _ as u64, address as u64)) else {
            tracing::warn!(dll, %symbol, "no room for a counting stub; bound without it");
            return found;
        };
        watch.counted.push(Counted {
            dll: dll.to_owned(),
            symbol,
            calls,
            stub,
        });
        Resolution::Found {
            address: stub,
            provenance: Provenance::Title,
        }
    }
}

/// Copies `code` into executable memory of its own and returns where.
/// Leaked: the title holds the address for the life of the process. One
/// allocation each, so no stub's page is ever writable while another stub
/// on it can run; only a few dozen are ever made.
#[cfg(windows)]
fn place(code: &[u8; STUB]) -> Option<usize> {
    use windows_sys::Win32::System::Diagnostics::Debug::FlushInstructionCache;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READ, PAGE_READWRITE, VirtualAlloc, VirtualProtect,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a fresh anonymous allocation, written before it is made
    // executable and never written again.
    unsafe {
        let at = VirtualAlloc(
            std::ptr::null(),
            STUB,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if at.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(code.as_ptr(), at.cast::<u8>(), STUB);
        let mut old = 0;
        if VirtualProtect(at, STUB, PAGE_EXECUTE_READ, &mut old) == 0 {
            return None;
        }
        FlushInstructionCache(GetCurrentProcess(), at, STUB);
        Some(at as usize)
    }
}

#[cfg(not(windows))]
fn place(_: &[u8; STUB]) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stub_counts_then_jumps() {
        let code = stub_code(0x1122_3344_5566_7788, 0x99AA_BBCC_DDEE_FF00);
        let want: Vec<u8> = [
            &[0x49, 0xBB][..],
            &0x1122_3344_5566_7788_u64.to_le_bytes(),
            &[0xF0, 0x49, 0xFF, 0x03],
            &[0x49, 0xBB],
            &0x99AA_BBCC_DDEE_FF00_u64.to_le_bytes(),
            &[0x41, 0xFF, 0xE3],
            &[0xCC; 5],
        ]
        .concat();
        assert_eq!(code.as_slice(), want.as_slice());
    }

    /// Runs real stubs: the call reaches the function with every argument
    /// and its result comes back, and each call is counted once.
    #[cfg(windows)]
    #[test]
    fn calls_through_a_stub_arrive_and_are_counted() {
        extern "system" fn six(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
            a + 10 * b + 100 * c + 1_000 * d + 10_000 * e + 100_000 * f
        }
        struct Title(usize);
        impl ImportResolver for Title {
            fn resolve(&self, _: &str, _: &ImportSymbol) -> Resolution {
                Resolution::Found {
                    address: self.0,
                    provenance: Provenance::Title,
                }
            }
        }
        let title = Title(six as *const () as usize);
        let name = |n: &str| ImportSymbol::Name {
            hint: 0,
            name: n.into(),
        };
        count_calls_into(&["watched.dll".into()]);
        let counting = Counting { inner: &title };
        let Resolution::Found { address, .. } = counting.resolve("WATCHED.dll", &name("six"))
        else {
            panic!("not found");
        };
        assert_ne!(address, title.0, "a stub in front");
        let again = counting.resolve("watched.dll", &name("six"));
        assert!(matches!(again, Resolution::Found { address: a, .. } if a == address));
        let other = counting.resolve("other.dll", &name("six"));
        assert!(matches!(other, Resolution::Found { address: a, .. } if a == title.0));
        let stub = address as *const ();
        // SAFETY: the stub jumps to `six`, whose signature this is.
        let through: extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64 =
            unsafe { std::mem::transmute(stub) };
        for _ in 0..3 {
            assert_eq!(through(1, 2, 3, 4, 5, 6), 654_321);
        }
        assert_eq!(counted_calls(), vec![("WATCHED.dll!six".to_owned(), 3)]);
        count_calls_into(&[]);
        assert!(!counting_calls());
    }
}
