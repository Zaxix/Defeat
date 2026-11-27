//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content and by the translator
//! that translated it (its source fingerprint and settings, see `build`), so
//! a changed translator never serves an old translation. Entries go through
//! `victory-pipeline`'s content-addressed cache, which writes them
//! atomically; an entry that does not read back is translated again.
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too (entries of older builds
//! are ignored anyway).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{HostShader, StaticSampler, UserData};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent on [`HITS`] and on [`MISSES`] (reading the entry, or
/// translating and writing it), for `crate::pipeline::make`'s line.
static HIT_US: AtomicU64 = AtomicU64::new(0);
static MISS_US: AtomicU64 = AtomicU64::new(0);

/// Cache hits and misses since the start, and the microseconds each took.
pub(crate) fn costs() -> [u64; 4] {
    [&HITS, &MISSES, &HIT_US, &MISS_US].map(|c| c.load(Ordering::Relaxed))
}

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// What identifies the translation, besides the container: the shader
/// crate's fingerprint (its source, every switch it reads, the host facts the
/// device gave it, the loop limit, barrier waits and clean-up passes) and
/// this file's encoding of the result. Not the whole executable any more: a
/// rebuild that leaves the translator alone keeps its translations (each new
/// build used to translate the title's thousands of shaders again, which kept
/// the first minutes of every test run short of pipelines).
/// `VICTORY_SHADER_CACHE_KEY=exe` restores the old key (the executable's hash).
/// Worked out per lookup, so a translation made before the device said its
/// wave size is never filed under the later key.
fn build() -> [u8; 32] {
    static WARNED: std::sync::Once = std::sync::Once::new();
    let limit = victory_shader::emit::loop_limit();
    if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
        WARNED.call_once(|| tracing::warn!(limit, "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"));
    }
    static EXE: OnceLock<Option<[u8; 32]>> = OnceLock::new();
    let exe = EXE.get_or_init(|| {
        if std::env::var_os("VICTORY_SHADER_CACHE_KEY").is_some_and(|v| v == "exe") {
            match std::env::current_exe().and_then(std::fs::read) {
                Ok(bytes) => Some(*blake3::hash(&bytes).as_bytes()),
                Err(e) => {
                    tracing::warn!(error = %e, "cannot read our executable; keying on the translator");
                    None
                }
            }
        } else {
            None
        }
    });
    let mut h = blake3::Hasher::new();
    h.update(b"victory shader cache key 2\0");
    if let Some(exe) = exe {
        h.update(exe);
    }
    h.update(&without_bvh(&without_diagnostics(victory_shader::emit::translation_fingerprint())));
    // The list this file reads for the translator (`translate_listed`): the
    // shader crate's fingerprint does not see it.
    h.update(b"VICTORY_WAVE_WIDE_ONLY=");
    if let Some(value) = std::env::var_os("VICTORY_WAVE_WIDE_ONLY") {
        h.update(value.as_encoded_bytes());
    }
    h.update(b"\0VICTORY_WAVE_PAIRS_ONLY=");
    if let Some(value) = std::env::var_os("VICTORY_WAVE_PAIRS_ONLY") {
        h.update(value.as_encoded_bytes());
    }
    // The encoding of cache entries (`encode`, `decode`) is part of the
    // key. It was this file's whole text, so that any edit to this file
    // (a counter, a comment) made every cached translation a miss: run 3
    // on the owner's PC translated every shader again after one did. The
    // file's text at that edit (11169c3) is kept in `shader_cache.key` and
    // hashed instead, byte for byte as before (the repository's text files
    // are checked out with LF everywhere), so the caches made since stay
    // valid. Change that file (any edit will do) only when the encoding
    // changes.
    h.update(include_bytes!("shader_cache.key"));
    *h.finalize().as_bytes()
}

/// `fingerprint` without the values of `VICTORY_BVH_BUILDER` and
/// `VICTORY_BVH_NO_HIT`: they change only the programs that test rays
/// against ray-tracing structures, which [`cached`] keys on their effect
/// (`victory_shader::emit::bvh_no_hit`). With them in the build key a run
/// with the structure builder on translated all of the title's thousands
/// of programs again, for the hundred or so that test rays.
fn without_bvh(fingerprint: &[u8]) -> Vec<u8> {
    let mut out = fingerprint.to_vec();
    for name in [&b"VICTORY_BVH_BUILDER="[..], &b"VICTORY_BVH_NO_HIT="[..]] {
        if let Some(at) = out.windows(name.len()).position(|w| w == name) {
            let after = at + name.len();
            let end = out[after..].iter().position(|&b| b == 0).map_or(out.len(), |n| after + n);
            out.drain(after..end);
        }
    }
    out
}

/// `fingerprint` without the values of the translator's diagnostic switches
/// (`VICTORY_*_WHY`, which only print why a form was or was not taken):
/// setting one made every cached translation a miss, so a diagnostics run
/// translated every shader again. Unset, the fingerprint is unchanged
/// (the name stays, its empty value too), so the caches made before stay
/// valid. Each switch is `NAME=value\0` in the fingerprint.
fn without_diagnostics(fingerprint: Vec<u8>) -> Vec<u8> {
    const WHY: &[u8] = b"_WHY=";
    let mut out = Vec::with_capacity(fingerprint.len());
    let mut rest = &fingerprint[..];
    while let Some(at) = rest.windows(WHY.len()).position(|w| w == WHY) {
        let after = at + WHY.len();
        out.extend_from_slice(&rest[..after]);
        let end = rest[after..].iter().position(|&b| b == 0).map_or(rest.len(), |n| after + n);
        rest = &rest[end..];
    }
    out.extend_from_slice(rest);
    out
}

/// Whether container `c`'s hash is on `VICTORY_SHADER_FRESH` (16-hex-digit
/// hashes as the log names shaders, comma separated): translated anew, not
/// read from the cache (nor written to it), so that the translator's
/// diagnostics (`VICTORY_WAVE_WHY`, `VICTORY_DESC_WHY`) are printed for
/// shaders the cache holds. The translation is the same.
fn fresh(c: &[u8]) -> bool {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    let list = LIST.get_or_init(|| {
        let list: Vec<String> = std::env::var("VICTORY_SHADER_FRESH")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect();
        if !list.is_empty() {
            tracing::info!(shaders = list.len(), "shaders translated anew, not from the cache (VICTORY_SHADER_FRESH)");
        }
        list
    });
    if list.is_empty() && !pairs_by_default() {
        return false;
    }
    let hash = blake3::hash(c).to_hex();
    // The default pairs list's shaders: the cache holds their old 64-lane
    // translation (keyed as before, the list being unset), which the card
    // refuses; they are translated anew at each launch instead.
    listed_pair_hash(hash.as_str(), list) || (pairs_by_default() && listed_pair_hash(hash.as_str(), pairs_list()))
}

/// The compute programs the owner's RTX 4070 refused in runs 10 and 11 (28,
/// the same in both), every one asking for 64-lane waves (the card runs
/// 32); the water's two are 5dc3b25986622949 and 6f225b641c5b0708: the paired form
/// (`victory_shader::emit::with_wave_pairs`, pull request #854 with the
/// gta6 base's exact reductions) is tried for them first by default. It
/// runs a console wave of 64 lanes as one host wave of 32 lanes, each
/// lane doing two, so every wave operation sees all 64 lanes: exact, at
/// twice the work a lane. Where it is refused (its reason is logged as
/// `WAVE64-PAIRS-ONLY refused`) the usual forms follow, as before. The
/// base made the water's two programs (the last two) right this way, with
/// `VICTORY_WAVE_PAIRS_FP32=non-ieee-rne-ftz` for their float reductions.
/// A list of the refused alone, by hash: the form is not tried for any
/// program that works now (R55: not globally).
const PAIRS_BY_DEFAULT: [&str; 28] = [
    "02384a60fda6f2d0",
    "18d957e62926ed3e",
    "1d434307d981352a",
    "22338086ea4c5364",
    "2889c21328c69e58",
    "2da4beb386509b77",
    "369cbe906ea72764",
    "3b02415f67c6b0d8",
    "407eca669212d400",
    "57f1b81de55c2842",
    "5dc3b25986622949",
    "5e3dd99e0f067aee",
    "6832a9da15c2e584",
    "6871d6e4b3ac9a10",
    "6b82bc7d22016cb5",
    "6f225b641c5b0708",
    "888af37a584acf98",
    "926c46bab3b86bd6",
    "98fef92f7a6d825e",
    "9fe711566bec0d0a",
    "a09f0c1fae2b5ca3",
    "a10179ab7eda0dc3",
    "a40ae27594631121",
    "a4cb3ce527ad8569",
    "ae0deeafe219c15c",
    "aeac9d880daff3ba",
    "f38f83439083b9ef",
    "ffa3a7645374474b",
];

/// Whether [`PAIRS_BY_DEFAULT`] is the pairs list: `VICTORY_WAVE_PAIRS_ONLY`
/// unset. (Set, it is the list; `none` is none.)
fn pairs_by_default() -> bool {
    std::env::var_os("VICTORY_WAVE_PAIRS_ONLY").is_none()
}

/// The pairs list in force: `VICTORY_WAVE_PAIRS_ONLY`, or
/// [`PAIRS_BY_DEFAULT`] when that is unset.
fn pairs_list() -> &'static [String] {
    static LIST: OnceLock<Vec<String>> = OnceLock::new();
    LIST.get_or_init(|| parse_pairs(std::env::var("VICTORY_WAVE_PAIRS_ONLY").ok().as_deref()))
}

fn parse_pairs(value: Option<&str>) -> Vec<String> {
    match value {
        None => PAIRS_BY_DEFAULT.iter().map(|h| (*h).to_owned()).collect(),
        Some(v) => v
            .split(',')
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .collect(),
    }
}

/// A container translated with the wave-wide form allowed when its hash is
/// on `VICTORY_WAVE_WIDE_ONLY` (16-hex-digit hashes as the log names
/// shaders, comma separated; unset or empty: none). Off by default: two
/// programs that moved to that form were in a submission that lost the card
/// (runs 121-124, not proven the cause). After commit a416e392 of pull
/// request #823 (Felix2180), whose list is the four refused programs
/// 5dc3b25986622949 and 6f225b641c5b0708 (the water's transmission
/// volumetrics), 369cbe906ea72764 (particle visibility) and
/// 2cc4c197ee29e291 (the reflection pass's tiling). For runs and the offline
/// warm-up alike, which share the cache.
fn translate_listed(c: &[u8]) -> Result<HostShader, String> {
    static LIST: std::sync::OnceLock<Vec<String>> = std::sync::OnceLock::new();
    let list = LIST.get_or_init(|| {
        std::env::var("VICTORY_WAVE_WIDE_ONLY")
            .unwrap_or_default()
            .split(',')
            .map(|h| h.trim().to_lowercase())
            .filter(|h| !h.is_empty() && h != "none")
            .collect()
    });
    let hash = blake3::hash(c).to_hex();
    let wide = list.iter().any(|h| hash.starts_with(h.as_str()));
    let pairs = pairs_list();
    let paired = listed_pair_hash(hash.as_str(), pairs);
    if paired {
        eprintln!("WAVE64-PAIRS-ONLY selected {}", &hash.as_str()[..16]);
    }
    let done = victory_shader::emit::with_wave_wide(wide, || {
        victory_shader::emit::with_wave_pairs(paired, || victory_shader::emit::translate(c))
    })
    .map_err(|e| e.to_string());
    if let Err(e) = &done
        && e.contains("a descriptor read only as data")
    {
        descriptor_trace(&hash.as_str()[..16], c);
    }
    done
}

/// For a program that stops at "a descriptor read only as data" (five
/// compute programs in runs 5-11, all at an `s_load_dwordx4` from root
/// parameter 228 with an offset known only at run time): which of each
/// such load's four dwords the code reads, and by what, two steps on.
/// The translator gives those dwords a meaning only where it can tell what
/// they are (a buffer's record count is dword 2); the lines say what the
/// rest of the code wants from them. Once per program and launch, on
/// standard error as `DESC-TRACE <hash>` lines. Here and not in the
/// translator: a change to the translator's crate misses every cached
/// translation.
fn descriptor_trace(hash: &str, container: &[u8]) {
    static DONE: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    {
        let mut done = DONE.lock().unwrap_or_else(|e| e.into_inner());
        if done.iter().any(|h| h == hash) {
            return;
        }
        done.push(hash.to_owned());
    }
    let program = (|| {
        let c = victory_shader::Container::parse(container).ok()?;
        let dxil = c.part(victory_shader::FourCC::DXIL)?;
        let native = victory_shader::native_programs(dxil.data).ok()?.into_iter().find(|p| p.stage == HwStage::Cs)?;
        victory_shader::spill::build(&native).ok()
    })();
    let Some(p) = program else {
        eprintln!("DESC-TRACE {hash} the program could not be decoded again");
        return;
    };
    for line in descriptor_uses(&p) {
        eprintln!("DESC-TRACE {hash} {line}");
    }
}

/// [`descriptor_trace`]'s lines for program `p`: every 4- or 8-dword scalar
/// load no image or buffer instruction uses as its descriptor, and for each of its dwords the instructions that read it, then
/// what reads theirs.
fn descriptor_uses(p: &victory_shader::ir::Program) -> Vec<String> {
    use victory_shader::ir::{Inst, Reg, Value};
    let insts: Vec<&Inst> = p.blocks.iter().flat_map(|b| &b.insts).collect();
    let reg = |r: &Reg| match r {
        Reg::S(n) => format!("s{n}"),
        Reg::V(n) => format!("v{n}"),
        Reg::Scc => "scc".to_owned(),
    };
    // Values a phi carries on, as the value it makes.
    let through = |v: Value| -> Vec<Value> {
        p.blocks.iter().flat_map(|b| &b.phis).filter(|f| f.args.contains(&v)).map(|f| f.value).collect()
    };
    let readers = |v: Value| -> Vec<&Inst> {
        let mut vs = vec![v];
        vs.extend(through(v));
        insts.iter().copied().filter(|i| i.reads.iter().any(|r| vs.contains(&r.1))).collect()
    };
    let say = |i: &Inst| {
        let w: Vec<String> = i.writes.iter().map(|w| reg(&w.0)).collect();
        let r: Vec<String> = i.reads.iter().map(|r| reg(&r.0)).collect();
        format!("{:#x} {} [{} <- {}]", i.machine.offset, i.name(), w.join(","), r.join(","))
    };
    let mut out = Vec::new();
    for load in insts.iter().filter(|i| matches!(i.name(), "s_load_dwordx4" | "s_load_dwordx8" | "s_buffer_load_dwordx4" | "s_buffer_load_dwordx8")) {
        // A load some instruction uses as a descriptor is not the one.
        let as_descriptor = load.writes.iter().flat_map(|w| readers(w.1)).any(|i| {
            ["image_", "buffer_", "tbuffer_", "s_buffer_"].iter().any(|k| i.name().starts_with(k))
        });
        if as_descriptor {
            continue;
        }
        out.push(format!("load {}", say(load)));
        for (k, (r, v)) in load.writes.iter().enumerate() {
            let first = readers(*v);
            if first.is_empty() {
                out.push(format!("  dword {k} ({}) read by nothing", reg(r)));
                continue;
            }
            for i in first.iter().take(8) {
                let next: Vec<String> =
                    i.writes.iter().flat_map(|w| readers(w.1)).take(4).map(|j| format!("{:#x} {}", j.machine.offset, j.name())).collect();
                out.push(format!("  dword {k} ({}) -> {} -> [{}]", reg(r), say(i), next.join("; ")));
            }
        }
    }
    out
}

fn listed_pair_hash(hash: &str, list: &[String]) -> bool {
    hash.get(..16).is_some_and(|prefix| list.iter().any(|h| h == prefix))
}

#[cfg(test)]
#[test]
fn a_ray_testing_program_is_translated_from_its_container() {
    // The input holds the build key and the ray-test key before the
    // container; the converter is handed the container alone.
    let seen = std::sync::Mutex::new(Vec::new());
    let converter = Translate {
        translate: |c: &[u8]| {
            seen.lock().unwrap().extend_from_slice(c);
            Err("stop".to_owned())
        },
        ran: AtomicBool::new(false),
        container_at: 32 + b"bvh no hit".len(),
    };
    let input = [&[7u8; 32][..], b"bvh no hit", b"DXBC..."].concat();
    assert!(converter.convert(&input).is_err());
    assert_eq!(&seen.lock().unwrap()[..], b"DXBC...");
}

#[test]
fn the_builder_switches_leave_the_build_key_alone() {
    let unset = b"HASH\x00\x01VICTORY_BVH_BUILDER=\0VICTORY_BVH_NO_HIT=\0VICTORY_GROUP_SYNC=2\0tail".to_vec();
    let set = b"HASH\x00\x01VICTORY_BVH_BUILDER=0\0VICTORY_BVH_NO_HIT=1\0VICTORY_GROUP_SYNC=2\0tail".to_vec();
    assert_eq!(without_bvh(&set), unset);
    assert_eq!(without_bvh(&unset), unset);
    let other = b"HASH\x00\x01VICTORY_BVH_BUILDER=0\0VICTORY_BVH_NO_HIT=\0VICTORY_GROUP_SYNC=0\0tail".to_vec();
    assert_ne!(without_bvh(&other), unset);
}

#[test]
fn diagnostic_switches_leave_the_fingerprint_as_unset() {
    let unset = b"HASH\x00\x01VICTORY_DESC_WHY=\0VICTORY_GROUP_SYNC=2\0VICTORY_WAVE_WHY=\0tail".to_vec();
    let set = b"HASH\x00\x01VICTORY_DESC_WHY=1\0VICTORY_GROUP_SYNC=2\0VICTORY_WAVE_WHY=yes\0tail".to_vec();
    assert_eq!(without_diagnostics(unset.clone()), unset, "unset: unchanged, so old caches stay valid");
    assert_eq!(without_diagnostics(set), unset);
    // Other switches still count.
    let other = b"HASH\x00\x01VICTORY_DESC_WHY=\0VICTORY_GROUP_SYNC=0\0VICTORY_WAVE_WHY=\0tail".to_vec();
    assert_ne!(without_diagnostics(other), unset);
}

#[cfg(test)]
#[test]
fn the_pairs_list_is_the_refused_programs_unless_set() {
    let default = parse_pairs(None);
    assert_eq!(default.len(), 28);
    assert!(listed_pair_hash("369cbe906ea72764ffff", &default));
    assert!(listed_pair_hash("6f225b641c5b0708", &default));
    assert!(parse_pairs(Some("none")).is_empty());
    assert!(parse_pairs(Some("")).is_empty());
    assert_eq!(parse_pairs(Some("5DC3B25986622949, bad")), vec!["5dc3b25986622949".to_owned()]);
}

#[cfg(test)]
#[test]
fn listed_pair_hash_matches_only_complete_log_hashes() {
    let list = vec!["5dc3b25986622949".to_owned(), "6f225b641c5b0708".to_owned()];
    assert!(listed_pair_hash("5dc3b25986622949ffffffffffffffffffffffffffffffff", &list));
    assert!(listed_pair_hash("6f225b641c5b0708", &list));
    assert!(!listed_pair_hash("5dc3b25986622940", &list));
    assert!(!listed_pair_hash("5dc3", &list));
    assert!(!listed_pair_hash("5dc3b25986622949", &["5dc3".to_owned()]));
}

#[cfg(test)]
#[test]
fn listed_pair_cache_key_changes() {
    const CHILD: &str = "VICTORY_CACHE_PAIRS_KEY_CHILD";
    if std::env::var_os(CHILD).is_some() {
        println!("pairs-cache-key={}", blake3::Hash::from_bytes(build()));
        return;
    }
    let key = |list: &str| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["listed_pair_cache_key_changes", "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .env("VICTORY_WAVE_PAIRS", "0")
            .env("VICTORY_WAVE_PAIRS_ONLY", list)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().lines()
            .find_map(|line| line.split_once("pairs-cache-key=").map(|(_, key)| key.to_owned()))
            .expect("the child printed its cache key")
    };
    let off = key("");
    assert_ne!(off, key("5dc3b25986622949"));
    assert_ne!(key("5dc3b25986622949"), key("6f225b641c5b0708"));
    assert_eq!(off, key(""));
}

/// The offline warm-up (`victory translate --warm`): `container` is
/// translated into the cache unless it is there already. `Ok(true)`: it
/// was translated now. Needs [`use_shader_cache`] and the same host facts
/// the run will have (`victory_shader::emit::translation_fingerprint`).
pub fn warm(container: &[u8]) -> Result<bool, String> {
    let cache = CACHE.get().ok_or("no shader cache")?;
    let ran = AtomicBool::new(false);
    let translate = |c: &[u8]| {
        ran.store(true, Ordering::Relaxed);
        translate_listed(c)
    };
    cached(cache, &build(), container, translate).map(|_| ran.load(Ordering::Relaxed))
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8]) -> Result<HostShader, String> {
    let translate = translate_listed;
    if fresh(container) {
        let hash = blake3::hash(container).to_hex();
        // One at a time, so that the translator's diagnostic lines
        // (`VICTORY_WAVE_WHY`, `VICTORY_DESC_WHY`) of one shader sit between
        // its two lines here: in run 9 several threads translated at once
        // and their lines interleaved (R51). (The translator's own crate is
        // left alone: an edit to it misses every cached translation.)
        static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
        let short = &hash.as_str()[..16];
        eprintln!("SHADER-FRESH {short} translating anew (VICTORY_SHADER_FRESH, or the default pairs list)");
        let made = translate(container);
        match &made {
            Ok(_) => eprintln!("SHADER-FRESH {short} done: translated"),
            Err(e) => eprintln!("SHADER-FRESH {short} done: {e}"),
        }
        return made;
    }
    match CACHE.get() {
        Some(cache) => cached(cache, &build(), container, translate),
        None => translate(container),
    }
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
    /// Where the container starts in the input: after the build key and
    /// the ray-test key ([`cached`]). Run 13 translated from the build
    /// key's end, so the 64 programs that test rays were handed their key
    /// with the container ("not a DXBC container") and none translated.
    container_at: usize,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // The build fingerprint in the input does the versioning.
        1
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[self.container_at..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader))
    }
}

fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let started = std::time::Instant::now();
    // A program that tests rays is translated as the structure builder
    // goes in this run ([`without_bvh`]).
    let bvh: &[u8] = if crate::pipeline::container_walks(container) {
        if victory_shader::emit::bvh_no_hit() { b"bvh no hit" } else { b"bvh tested" }
    } else {
        b""
    };
    let input = [&build[..], bvh, container].concat();
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
        container_at: build.len() + bvh.len(),
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(shader) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let (counter, time, what) = if converter.ran.load(Ordering::Relaxed) {
                (&MISSES, &MISS_US, "translated and cached")
            } else {
                (&HITS, &HIT_US, "from the cache")
            };
            counter.fetch_add(1, Ordering::Relaxed);
            time.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            tracing::debug!(path = %path.display(), "shader {what}");
            return Ok(shader);
        }
        tracing::debug!(path = %path.display(), "unreadable shader cache entry; translating again");
        let _ = std::fs::remove_file(&path);
    }
    let done = (converter.translate)(container);
    MISSES.fetch_add(1, Ordering::Relaxed);
    MISS_US.fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
    done
}

const MAGIC: &[u8; 4] = b"VSH1";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    word(&mut out, s.half_views.len() as u32);
    for &(slot, dword) in &s.half_views {
        word(&mut out, u32::from(slot));
        word(&mut out, dword);
    }
    out
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    let n = r.word()? as usize;
    let mut half_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        half_views.push((u8::try_from(r.word()?).ok()?, r.word()?));
    }
    r.0.is_empty().then_some(HostShader {
        stage,
        dxil,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        root_indexed_views,
        half_views,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        HostShader {
            stage: HwStage::Ps,
            dxil: b"DXBC and the rest".to_vec(),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
            half_views: vec![(0, 8), (4, 0)],
        }
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH1 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        assert_eq!(decode(b"VSH0"), None);
    }
}
