//! Host pipelines are made by workers, not by the title thread that first
//! records a draw with them. Translating a shader and the host driver's
//! compile take from milliseconds to seconds each (structurisation of the
//! biggest compute programs ~0.4 s, some driver compiles 3.4 s), and
//! the title's first world frame wants over 1,200 new pipelines: made
//! on its recording threads, that frame took more than 30 s and the
//! title's GPU watchdog ended the run (decision 0007, "Pipelines made off
//! the title's threads").
//!
//! A first use queues the pipeline and waits for it within a budget per
//! frame, so the cheap ones (menus, one-off passes such as a colour-grading
//! bake) are there at once as before; past the budget a draw or dispatch
//! whose pipeline is not ready is dropped, as an untranslated one is.
//!
//! The default budget is half a second per title frame. A pass the title
//! runs only once and whose pipeline takes longer is lost.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::Shader;
use super::host::{GraphicsState, HostPipeline, TargetFormats};

/// `VICTORY_DENY_PIPELINES=<16-hex shader hash>,...` (off unless set): the
/// hash of a shader of `shaders` that is listed there. Such a pipeline is
/// never built (no driver compile) and counts as refused, so its draws and
/// dispatches are dropped: a test of whether building a pipeline, rather
/// than running it, is what loses the card (runs 281 and 318: the removal
/// 32 and 81 ms after the 3-second build of `CS_RaytracingResolve` of the
/// two-layer vehicle wheel material, 57a3fad03cac36af and d5b9c94a30d7cf28;
/// gfx41). Unlike `VICTORY_SKIP_PIPELINES`, in release builds too.
fn denied(shaders: &[Arc<Shader>]) -> Option<&str> {
    static LIST: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    let list = LIST.get_or_init(|| {
        // `VICTORY_DENY_PIPELINES_FILE=<path>`: more hashes, any 16 hex
        // digits in the file (one a line, say), with the list's own.
        let file = std::env::var_os("VICTORY_DENY_PIPELINES_FILE")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let list: std::collections::HashSet<String> = std::env::var("VICTORY_DENY_PIPELINES")
            .unwrap_or_default()
            .split(|c: char| !c.is_ascii_hexdigit())
            .chain(file.split(|c: char| !c.is_ascii_hexdigit()))
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| h.len() == 16)
            .collect();
        if !list.is_empty() {
            tracing::info!(pipelines = list.len(), "shaders whose pipelines are not built (VICTORY_DENY_PIPELINES / _FILE)");
        }
        list
    });
    if list.is_empty() {
        return None;
    }
    shaders.iter().find(|s| list.contains(&s.hash)).map(|s| s.hash.as_str())
}

/// The shaders of `p` (for logs).
pub(crate) fn shaders(p: &Pending) -> &[Arc<Shader>] {
    &p.shaders
}

/// What a host pipeline is made from, and the host pipeline once made.
pub(crate) struct Pending {
    shaders: Vec<Arc<Shader>>,
    pub(crate) graphics: Option<Box<GraphicsState>>,
    queue_state: AtomicU8,
    made: OnceLock<Option<HostPipeline>>,
    /// What identifies it across runs (shader containers and state), and
    /// whether a draw or dispatch has requested it yet.
    key: String,
    used: AtomicBool,
    /// Whether the made pipeline has gone to a draw or dispatch yet
    /// (`crate::host::first_use`: for a device removal's report).
    handed: AtomicBool,
    /// The formats its state declares for what a draw draws into (zeros
    /// without a graphics state).
    pub(crate) formats: TargetFormats,
    /// The same shaders and state for other formats, bound at a draw
    /// ([`for_formats`]): a chain from the title's pipeline, one link per
    /// set of formats met, never shortened.
    variant: OnceLock<Arc<Pending>>,
}

// SAFETY: D3D12 objects are free-threaded; the graphics state (whose input
// elements point into its own names) is only read after creation.
unsafe impl Send for Pending {}
// SAFETY: as above.
unsafe impl Sync for Pending {}

impl Pending {
    pub(crate) fn new(shaders: Vec<Arc<Shader>>, graphics: Option<Box<GraphicsState>>) -> Pending {
        let mut key = blake3::Hasher::new();
        for s in &shaders {
            key.update(s.stage.as_bytes());
            key.update(s.hash.as_bytes());
        }
        key.update(graphics.as_ref().map(|g| g.describe()).unwrap_or_default().as_bytes());
        Pending {
            formats: graphics.as_ref().map_or([0; 9], |g| g.formats()),
            variant: OnceLock::new(),
            shaders,
            graphics,
            queue_state: AtomicU8::new(NOT_QUEUED),
            made: OnceLock::new(),
            key: key.finalize().to_hex()[..32].to_owned(),
            used: AtomicBool::new(false),
            handed: AtomicBool::new(false),
        }
    }

    fn make(&self) {
        self.make_with(|| {
            if let Some(hash) = denied(&self.shaders) {
                tracing::warn!(hash, shaders = ?self.shaders, "pipeline not built: its shader is in VICTORY_DENY_PIPELINES");
                return None;
            }
            let started = Instant::now();
            let made = HostPipeline::new(&self.shaders, self.graphics.as_deref());
            made_in(started.elapsed());
            made
        });
    }

    fn make_with(&self, build: impl FnOnce() -> Option<HostPipeline>) {
        self.made.get_or_init(|| {
            // A failed translation must complete this queue entry and wake its waiters.
            match catch_unwind(AssertUnwindSafe(build)) {
                Ok(pipeline) => pipeline,
                Err(_) => {
                    tracing::warn!(shaders = ?self.shaders, "host pipeline build panicked");
                    None
                }
            }
        });
        let (lock, done) = &DONE;
        let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
        done.notify_all();
    }
}

/// Why a pipeline has no host pipeline (diagnostics): its shaders that do
/// not translate, else "refused" or "being made"; `None` if it has one.
#[cfg(feature = "diagnostics")]
pub(crate) fn missing(p: &Pending) -> Option<String> {
    match p.made.get() {
        Some(Some(_)) => None,
        None => Some("being made".into()),
        Some(None) => {
            let failed: Vec<String> = p
                .shaders
                .iter()
                .filter(|s| matches!(s.translated.get(), Some(None)))
                .map(|s| format!("{} {}", s.stage, s.hash))
                .collect();
            Some(if failed.is_empty() {
                format!("refused {:?}", p.shaders)
            } else {
                failed.join(" + ")
            })
        }
    }
}

/// Signalled whenever a worker has made a pipeline.
static DONE: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Waiting allowed per title frame, over all threads.
fn wait_budget() -> Duration {
    static BUDGET: OnceLock<Duration> = OnceLock::new();
    *BUDGET.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(500);
        tracing::info!(millis, "host pipeline wait budget per frame");
        Duration::from_millis(millis)
    })
}

/// Pipelines queued and not made yet.
static WAITING: AtomicU64 = AtomicU64::new(0);

/// Pipelines queued and not made yet (for the frame line).
pub(crate) fn waiting() -> u64 {
    WAITING.load(Ordering::Relaxed)
}

/// The host pipeline if it is made (or is made within the frame's budget);
/// queues it on first use.
pub(crate) fn ready(p: &Arc<Pending>) -> Option<&HostPipeline> {
    // A still-pending pipeline was requested by the title too. Prewarm it
    // on the next launch, even if this run never got to use it.
    if !p.used.load(Ordering::Relaxed) && !p.used.swap(true, Ordering::Relaxed) {
        super::library::note_used(&p.key);
    }
    if p.made.get().is_none() {
        start(p, REQUESTED);
        // A compute pipeline in the first 600 frames (the title's one-shot
        // world and lighting set-up passes) or with `compute_waits`: made
        // here, or waited for while a worker makes it, instead of dropping
        // its dispatch after the frame's budget. `make` keeps its panic guard.
        let frame = crate::frame::current();
        // A worker makes it and the title's thread waits for it, for at most
        // what is left of the frame's limit: made on the title's thread, one
        // compile took 14.6 s on the owner's PC (run 4, frame 2221), past a
        // limit that was only asked before each compile.
        if p.graphics.is_none()
            && (compute_waits() || frame < 600)
            && let Some(left) = compute_wait_left(frame)
        {
            let started = Instant::now();
            wait_compute(p, left);
            compute_waited(frame, started.elapsed());
        } else {
            wait(p, frame);
        }
    }
    let made = match p.made.get() {
        None => {
            MISSED.set(MISS_MAKING);
            return None;
        }
        Some(None) => {
            MISSED.set(MISS_REFUSED);
            return None;
        }
        Some(Some(made)) => made,
    };
    // The first time the made pipeline goes to a draw or dispatch: noted
    // for a device removal's report (one load a call after that).
    if !p.handed.load(Ordering::Relaxed) && !p.handed.swap(true, Ordering::Relaxed) {
        crate::host::first_use(&made.name);
    }
    Some(made)
}

/// [`last_miss`]: the pipeline is still being made.
pub(crate) const MISS_MAKING: u8 = 1;
/// [`last_miss`]: the host refused it, or a shader of it did not translate.
pub(crate) const MISS_REFUSED: u8 = 2;

thread_local! {
    static MISSED: std::cell::Cell<u8> = const { std::cell::Cell::new(0) };
}

/// Why the last [`ready`] on this thread found no host pipeline
/// ([`MISS_MAKING`], [`MISS_REFUSED`]; 0 before any): for the frame line's
/// dropped draws by why, in release builds too.
pub(crate) fn last_miss() -> u8 {
    MISSED.get()
}

/// Waits for compute pipeline `p` for at most `left`: at the front of the
/// workers' queue if no worker has taken it yet, so that the next free
/// worker makes it; made on this thread if there are no workers.
/// Before (run 4), the title's thread made it itself: a limit asked only
/// before each compile, and one thread compiling while the workers had
/// the cores.
fn wait_compute(p: &Arc<Pending>, left: Duration) {
    if !workers() {
        p.make();
        return;
    }
    {
        let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
        queue.hurry(p);
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// Whether a dispatch waits for its compute pipeline until it is made,
/// whatever the frame's budget ([`wait_budget`]); a draw still waits within
/// the budget and is dropped while its pipeline is not there.
///
/// A dropped draw is drawn again in the next frame. A dropped dispatch may
/// not be: the title runs some compute passes once, when a scene changes
/// (a buffer reset or cleared), and what they leave is read for as long as
/// the scene lasts. In the prison corridor a compute program met for the
/// first time took 4.4 s to make on a 16 GB machine; its one dispatch was
/// dropped after the half second of budget, and the windows of the room
/// before stayed blended over the whole picture for minutes (run 195).
/// A wait of seconds once (the pipeline library has it on the next run)
/// against a picture wrong until the scene ends.
/// On by default; `VICTORY_PIPELINE_COMPUTE_WAIT=0`: within the budget, as
/// before.
fn compute_waits() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_COMPUTE_WAIT").is_none_or(|v| v != "0"))
}

/// The most one frame waits in all for the compute pipelines it makes on
/// the title's thread ([`ready`]): past it, a dispatch whose pipeline is
/// not made waits within the frame's budget like a draw's and is dropped
/// while it is not there; a worker goes on making the pipeline, and the
/// next frame waits again. `VICTORY_PIPELINE_COMPUTE_WAIT_MS` (10,000 by
/// default; 0: no limit, as before).
///
/// Why: on the owner's PC (RTX 4070, cold caches, 2026-11-27) the first
/// world frame met some 820 shaders and 950 pipelines never seen before.
/// Its compute pipelines were made one after another on the title's
/// thread, no frame ended for about 30 s, and the title's own hang
/// watchdog stopped it with a write to address 0. A dispatch dropped once
/// may leave a one-shot pass undone for a scene (run 195's blended
/// windows); a frame held past the watchdog ends the game.
fn compute_wait_most() -> Option<Duration> {
    static MOST: OnceLock<Option<Duration>> = OnceLock::new();
    *MOST.get_or_init(|| {
        let millis = std::env::var("VICTORY_PIPELINE_COMPUTE_WAIT_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(10_000);
        tracing::info!(millis, "most a frame waits for compute pipelines (VICTORY_PIPELINE_COMPUTE_WAIT_MS; 0: no limit)");
        (millis != 0).then(|| Duration::from_millis(millis))
    })
}

/// The frame [`compute_wait_most`] is counted for, and what its making of
/// compute pipelines on the title's threads took so far (summed over
/// threads: a frame of several recording threads reaches the limit
/// sooner, never later).
static COMPUTE_SPENT: Mutex<(u64, Duration)> = Mutex::new((u64::MAX, Duration::ZERO));

/// What frame `frame` may still wait for compute pipelines
/// ([`compute_wait_most`]; an hour without a limit); `None` past it.
fn compute_wait_left(frame: u64) -> Option<Duration> {
    let Some(most) = compute_wait_most() else {
        return Some(Duration::from_secs(3600));
    };
    left_in(&COMPUTE_SPENT.lock().unwrap_or_else(|e| e.into_inner()), frame, most)
}

/// [`compute_wait_left`] on a count `spent` (frame, time) and limit `most`.
fn left_in(spent: &(u64, Duration), frame: u64, most: Duration) -> Option<Duration> {
    let left = if spent.0 == frame { most.saturating_sub(spent.1) } else { most };
    (!left.is_zero()).then_some(left)
}

/// Adds `took` to the count `spent` for frame `frame` (a new frame starts
/// it again); true if that took it to `most`.
fn add_to(spent: &mut (u64, Duration), frame: u64, took: Duration, most: Duration) -> bool {
    if spent.0 != frame {
        *spent = (frame, Duration::ZERO);
    }
    let before = spent.1;
    spent.1 += took;
    before < most && spent.1 >= most
}

/// Frame `frame` made a compute pipeline on the title's thread in `took`;
/// said once a frame when that reaches [`compute_wait_most`].
fn compute_waited(frame: u64, took: Duration) {
    let Some(most) = compute_wait_most() else {
        return;
    };
    let mut spent = COMPUTE_SPENT.lock().unwrap_or_else(|e| e.into_inner());
    if add_to(&mut spent, frame, took, most) {
        tracing::warn!(
            frame,
            waited_ms = spent.1.as_millis() as u64,
            "a frame waited this long for compute pipelines made for the first time; its other new ones are waited for within the \
             frame's budget and their dispatches dropped until made, so that the frame ends before the title's hang watchdog \
             (VICTORY_PIPELINE_COMPUTE_WAIT_MS)"
        );
    }
}

/// Host pipelines made since the start and the microseconds their making
/// took (translating their shaders included), on every thread.
static MADE_COST: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// A host pipeline took `took` to make: counted, and at most every 10 s a
/// line on the making since the last one: pipelines made and their time,
/// how much of it was translating shaders (from the shader cache, or
/// translated anew), the rest being the host's pipeline creation (the
/// driver's compile, or a load from the pipeline library), and the queue.
/// For the first world frames of a cold start, which wait on this.
fn made_in(took: Duration) {
    use std::sync::atomic::Ordering::Relaxed;
    MADE_COST[0].fetch_add(1, Relaxed);
    MADE_COST[1].fetch_add(took.as_micros() as u64, Relaxed);
    static LAST: Mutex<Option<(Instant, [u64; 6])>> = Mutex::new(None);
    let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let [hits, misses, hit_us, miss_us] = crate::shader_cache::costs();
    let totals = [MADE_COST[0].load(Relaxed), MADE_COST[1].load(Relaxed), hits, misses, hit_us, miss_us];
    let Some((at, before)) = *last else {
        *last = Some((now, totals));
        return;
    };
    let secs = now.duration_since(at).as_secs_f64();
    if secs < 10.0 {
        return;
    }
    *last = Some((now, totals));
    let d: [u64; 6] = std::array::from_fn(|i| totals[i].saturating_sub(before[i]));
    let ms = |us: u64| us as f64 / 1000.0;
    let each = |us: u64, n: u64| if n == 0 { 0.0 } else { ms(us) / n as f64 };
    let translating = d[4] + d[5];
    tracing::info!(
        "pipelines made in the last {secs:.0} s: {} ({:.1} a second), {:.0} ms each on average on all threads; of their time {:.0} % \
         translating shaders ({} from the shader cache at {:.1} ms each, {} translated anew at {:.0} ms each), the rest the host's \
         pipeline creation (the driver, or the pipeline library); {} queued now",
        d[0],
        d[0] as f64 / secs,
        each(d[1], d[0]),
        if d[1] == 0 { 0.0 } else { 100.0 * translating as f64 / d[1] as f64 },
        d[2],
        each(d[4], d[2]),
        d[3],
        each(d[5], d[3]),
        WAITING.load(Relaxed)
    );
}

/// Pipelines made again for the formats bound at a draw, since the start.
static VARIANTS: AtomicU64 = AtomicU64::new(0);

/// Whether a draw into views of other formats than its pipeline declares
/// gets a host pipeline for those formats ([`for_formats`]). On by
/// default; `VICTORY_TARGET_VARIANTS=0` sends such a draw to the host with
/// the title's pipeline, as before.
fn variants_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TARGET_VARIANTS").is_none_or(|v| v != "0"))
}

/// The host pipeline for a draw with `p` into views of the formats
/// `bound`, which `p` does not declare (`host::formats_differ`): that of
/// the same shaders and state for those formats. It is a pipeline like the
/// title's own: queued at its first draw, made by a worker, waited for
/// within the frame's budget, and the draw is dropped while it is not
/// there ([`ready`]).
#[cold]
pub(crate) fn for_formats<'a>(p: &'a Arc<Pending>, bound: &TargetFormats) -> Option<&'a HostPipeline> {
    match p.graphics.as_deref() {
        Some(g) if variants_on() => ready(variant(p, g, &super::host::formats_for(&p.formats, bound))),
        _ => ready(p),
    }
}

/// The pipeline of `p`'s shaders and its state `g` for the formats
/// `bound`: found in the chain that starts at `p`, or added to it (logged
/// then: once per pipeline and set of formats).
fn variant<'a>(p: &'a Arc<Pending>, g: &GraphicsState, bound: &TargetFormats) -> &'a Arc<Pending> {
    let mut at = p;
    // (`bound` is what `host::formats_for` gives, and a state made for it
    // declares exactly it, so the walk ends at the link made for it; the
    // count is a bound all the same: past it the title's own pipeline.)
    for _ in 0..64 {
        let next = at.variant.get_or_init(|| {
            let so_far = VARIANTS.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::info!(
                pipeline = ?p.shaders,
                declared = ?p.formats,
                bound = ?bound,
                so_far,
                "a pipeline is made again for the render target formats bound at its draw (eight render targets, then depth; VICTORY_TARGET_VARIANTS=0: the draw goes out with the title's pipeline)"
            );
            Arc::new(Pending::new(p.shaders.clone(), Some(Box::new(g.with_formats(bound)))))
        });
        // (A state made for `bound` declares exactly `bound`.)
        if next.formats == *bound {
            return next;
        }
        at = next;
    }
    p
}

/// Makes `p` on this thread if it is not made yet (waiting for a worker
/// that is making it); whether it has a host pipeline.
pub(crate) fn now(p: &Arc<Pending>) -> bool {
    p.make();
    p.made.get().is_some_and(|made| made.is_some())
}

/// At the title's creation of a pipeline: starts it at once if an earlier
/// run drew with it (during loading, so the world's first frames find it),
/// and a compute pipeline in any case ([`compute_early`]).
pub(crate) fn early(p: &Arc<Pending>) {
    if p.shaders.is_empty() {
        return;
    }
    if super::library::wanted_early(&p.key) {
        super::library::EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    } else if p.graphics.is_none() && compute_early() {
        COMPUTE_EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, EARLY);
    }
}

/// Compute pipelines started at their creation that no earlier run used.
pub(crate) static COMPUTE_EARLY: AtomicU64 = AtomicU64::new(0);

/// Whether a compute pipeline no earlier run used is started at its
/// creation (behind the queue of requested ones, as the library's are),
/// not at its first dispatch.
///
/// Why: the title creates its compute pipelines while it loads and runs
/// many of them first in a scene's opening frames, where each was made
/// then, one after another, while the frame waited. In run 4 on the
/// owner's PC (RTX 4070, 5800X) the first cutscene frames waited 13.6 and
/// 14.6 s for four compute pipelines created a minute before (programs of
/// 12 to 66 KB): the 12 workers had been idle. Compute pipelines are the
/// fewer and the larger (835 of the run's 2,229 first binds; graphics
/// ones stay started at first use, being many more and cheap). The made
/// ones go to the pipeline library as any other.
/// On by default; `VICTORY_PIPELINE_COMPUTE_EARLY=0`: at first dispatch,
/// as before.
fn compute_early() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let on = std::env::var_os("VICTORY_PIPELINE_COMPUTE_EARLY").is_none_or(|v| v != "0");
        tracing::info!(on, "compute pipelines started at their creation (VICTORY_PIPELINE_COMPUTE_EARLY)");
        on
    })
}

/// Whether the frame's budget is one for all threads together: a thread
/// that waits takes what is left of it and gives back what it did not
/// use. Counted only when a wait had ended, the budget was one per thread:
/// each of the title's five recording threads began its first wait of a
/// frame with nothing counted yet and waited the whole half second, 2.5 s
/// of waiting in every frame in which pipelines were being made (runs
/// 34-49: "2504 ms waiting for pipelines" in each such frame).
/// On by default; `VICTORY_PIPELINE_WAIT_SHARED=0` counts as before.
fn shared_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PIPELINE_WAIT_SHARED").is_none_or(|v| v != "0"))
}

/// Waits for `p` while frame `frame` has budget left.
fn wait(p: &Pending, frame: u64) {
    let budget = wait_budget();
    if budget.is_zero() {
        return;
    }
    // The frame the budget belongs to, and what was waited of it (or is
    // taken by a thread waiting now).
    static SPENT: Mutex<(u64, Duration)> = Mutex::new((u64::MAX, Duration::ZERO));
    let shared = shared_budget();
    let left = {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        if spent.0 != frame {
            *spent = (frame, Duration::ZERO);
        }
        let left = budget.saturating_sub(spent.1);
        if shared {
            spent.1 += left;
        }
        left
    };
    if left.is_zero() {
        return;
    }
    let started = Instant::now();
    let (lock, done) = &DONE;
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    while p.made.get().is_none() {
        let rest = left.saturating_sub(started.elapsed());
        if rest.is_zero() {
            break;
        }
        held = done.wait_timeout(held, rest).unwrap_or_else(|e| e.into_inner()).0;
    }
    drop(held);
    let waited = started.elapsed().min(left);
    {
        let mut spent = SPENT.lock().unwrap_or_else(|e| e.into_inner());
        // A frame that began meanwhile has its own budget.
        if spent.0 == frame {
            if shared {
                spent.1 = spent.1.saturating_sub(left - waited);
            } else {
                spent.1 += waited;
            }
        }
    }
    crate::framelog::add(crate::framelog::T::PipelineWaitUs, started.elapsed().as_micros() as u64);
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(
        crate::copystat::C::PipelineWaitUs,
        started.elapsed().as_micros() as u64,
    );
}

const NOT_QUEUED: u8 = 0;
const EARLY: u8 = 1;
const REQUESTED: u8 = 2;
const MAKING: u8 = 3;

struct Queue {
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
}

impl Queue {
    const fn new() -> Self {
        Self {
            early: VecDeque::new(),
            requested: VecDeque::new(),
        }
    }

    /// Queues an unrequested pipeline or promotes one queued at creation.
    /// Returns whether it is new to the queue, or `None` if nothing changed.
    fn push(&mut self, p: &Arc<Pending>, priority: u8) -> Option<bool> {
        let prev = p.queue_state.load(Ordering::Relaxed);
        if prev >= priority {
            return None;
        }
        p.queue_state.store(priority, Ordering::Relaxed);
        if priority == REQUESTED {
            self.requested.push_back(p.clone());
        } else {
            self.early.push_back(p.clone());
        }
        Some(prev == NOT_QUEUED)
    }

    /// Moves requested pipeline `p` to the front of the queue if no worker
    /// has taken it yet.
    fn hurry(&mut self, p: &Arc<Pending>) {
        if p.queue_state.load(Ordering::Relaxed) != REQUESTED {
            return;
        }
        if let Some(at) = self.requested.iter().position(|q| Arc::ptr_eq(q, p))
            && at > 0
            && let Some(q) = self.requested.remove(at)
        {
            self.requested.push_front(q);
        }
    }

    fn pop(&mut self) -> Option<Arc<Pending>> {
        let p = self.requested.pop_front().or_else(|| {
            // An early entry can have been promoted and claimed already.
            std::iter::from_fn(|| self.early.pop_front())
                .find(|p| p.queue_state.load(Ordering::Relaxed) == EARLY)
        })?;
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some(p)
    }
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue::new());
static AVAILABLE: Condvar = Condvar::new();

/// Start or promote work for a draw without waiting behind the entire early
/// list. Queue changes and worker claims happen under the same lock.
fn start(p: &Arc<Pending>, priority: u8) {
    if p.queue_state.load(Ordering::Relaxed) >= priority {
        return;
    }
    if !workers() {
        if p.queue_state.swap(MAKING, Ordering::Relaxed) == NOT_QUEUED {
            p.make();
        }
        return;
    }
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(new) = queue.push(p, priority) {
        if new {
            PEAK.fetch_max(WAITING.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        AVAILABLE.notify_one();
    }
}

fn next() -> Arc<Pending> {
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(p) = queue.pop() {
            return p;
        }
        queue = AVAILABLE.wait(queue).unwrap_or_else(|e| e.into_inner());
    }
}

/// Starts the workers (the logical processors less 4, 4 to 14: 6 drained a
/// cold library 35 % slower than 8 in run 353) on first use, at normal priority.
fn workers() -> bool {
    static RUNNING: OnceLock<bool> = OnceLock::new();
    *RUNNING.get_or_init(|| {
        let mut running = false;
        // (12 on a Ryzen 7 5800X.) `VICTORY_PIPELINE_WORKERS` says how many
        // (1 to 64) instead: for a test of fewer next to the title's threads
        // and the storage workers (research checklist R31).
        let workers = std::env::var("VICTORY_PIPELINE_WORKERS")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map(|n| n.clamp(1, 64))
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(2, |n| n.get().saturating_sub(4).clamp(4, 14)));
        tracing::info!(workers, "pipeline workers (VICTORY_PIPELINE_WORKERS)");
        for i in 0..workers {
            let spawned = std::thread::Builder::new()
                .name(format!("victory-pipelines-{i}"))
                .spawn(|| {
                    loop {
                        next().make();
                        if WAITING.fetch_sub(1, Ordering::Relaxed) == 1 {
                            drained();
                        } else {
                            super::library::save(false);
                        }
                    }
                });
            if let Err(e) = spawned {
                tracing::warn!(error = %e, "no pipeline worker");
            } else {
                running = true;
            }
        }
        running
    })
}

/// The most pipelines waiting at once since the workers last ran out.
static PEAK: AtomicU64 = AtomicU64::new(0);

/// The workers ran out of work: logged after a batch of 10 or more, and
/// the cache saved (at most every 20 s).
fn drained() {
    use super::library::{EARLY, LOADED, MADE};
    if PEAK.swap(0, Ordering::Relaxed) >= 10 {
        tracing::info!(
            made = MADE.load(Ordering::Relaxed),
            loaded = LOADED.load(Ordering::Relaxed),
            started_early = EARLY.load(Ordering::Relaxed),
            compute_started_early = COMPUTE_EARLY.load(Ordering::Relaxed),
            "host pipelines all made"
        );
    }
    super::library::save(false);
}

#[cfg(test)]
mod tests {
    use super::{add_to, left_in};
    use std::time::Duration;

    /// A frame may make compute pipelines on the title's thread until it
    /// has spent the limit on them; the next frame may again.
    #[test]
    fn a_frame_stops_waiting_for_compute_pipelines_at_the_limit() {
        let most = Duration::from_secs(10);
        let mut spent = (u64::MAX, Duration::ZERO);
        assert_eq!(left_in(&spent, 7, most), Some(most));
        assert!(!add_to(&mut spent, 7, Duration::from_secs(9), most));
        assert_eq!(left_in(&spent, 7, most), Some(Duration::from_secs(1)), "what is left of the limit");
        assert!(add_to(&mut spent, 7, Duration::from_secs(2), most), "said once, when the limit is reached");
        assert_eq!(left_in(&spent, 7, most), None, "past the limit");
        assert!(!add_to(&mut spent, 7, Duration::from_secs(1), most), "not said again");
        assert_eq!(left_in(&spent, 8, most), Some(most), "a new frame waits again");
        assert!(!add_to(&mut spent, 8, Duration::from_millis(1), most));
        assert_eq!(left_in(&spent, 8, most), Some(most - Duration::from_millis(1)));
    }

    #[test]
    fn a_hurried_pipeline_goes_to_the_front_unless_taken() {
        let [a, b, c] = [(); 3].map(|()| Arc::new(Pending::new(Vec::new(), None)));
        let mut queue = Queue::new();
        for p in [&a, &b, &c] {
            assert_eq!(queue.push(p, REQUESTED), Some(true));
        }
        queue.hurry(&c);
        let order: Vec<_> = queue.requested.iter().map(Arc::as_ptr).collect();
        assert_eq!(order, [Arc::as_ptr(&c), Arc::as_ptr(&a), Arc::as_ptr(&b)]);
        let taken = queue.pop().expect("one queued");
        assert!(Arc::ptr_eq(&taken, &c));
        queue.hurry(&c);
        assert_eq!(queue.requested.len(), 2, "a taken pipeline is not queued again");
        // An early one hurried is left where it is (only requested ones are).
        let d = Arc::new(Pending::new(Vec::new(), None));
        assert_eq!(queue.push(&d, EARLY), Some(true));
        queue.hurry(&d);
        assert!(Arc::ptr_eq(queue.requested.front().expect("queued"), &a));
    }


    use super::*;

    #[test]
    fn a_panicking_pipeline_is_refused_and_the_worker_continues() {
        let bad = Arc::new(Pending::new(Vec::new(), None));
        let good = Arc::new(Pending::new(Vec::new(), None));
        let failed = bad.clone();
        let next = good.clone();
        let worker = std::thread::spawn(move || {
            failed.make_with(|| panic!("synthetic pipeline build panic"));
            next.make_with(|| None);
        });

        worker.join().expect("the pipeline worker should survive");
        assert!(matches!(bad.made.get(), Some(None)));
        assert!(matches!(good.made.get(), Some(None)));

        bad.make_with(|| panic!("the refused pipeline should stay complete"));
    }

    /// A first use queues the pipeline and, within the budget, gets the
    /// worker's answer (here: no host pipeline for no shaders).
    #[test]
    fn first_use_is_made_by_a_worker() {
        let p = Arc::new(Pending::new(Vec::new(), None));
        assert!(ready(&p).is_none());
        assert_ne!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        let started = Instant::now();
        while p.made.get().is_none() && started.elapsed() < Duration::from_secs(10) {
            std::thread::yield_now();
        }
        assert!(matches!(p.made.get(), Some(None)));
    }

    /// A draw into other formats than its pipeline declares finds the
    /// pipeline for them, which exists once per set of formats and is
    /// queued by its first draw like any other.
    #[test]
    fn a_pipeline_for_bound_formats_is_made_once() {
        // The title's pipeline declares one render target, format 24.
        let p = Arc::new(Pending::new(Vec::new(), Some(Box::new(GraphicsState::declaring(24)))));
        assert_eq!(p.formats, [24, 0, 0, 0, 0, 0, 0, 0, 0]);
        let g = p.graphics.as_deref().unwrap();
        let (a, b) = ([26, 0, 0, 0, 0, 0, 0, 0, 0], [28, 0, 0, 0, 0, 0, 0, 0, 0]);
        let first = variant(&p, g, &a);
        assert_eq!(first.formats, a);
        assert_ne!(first.key, p.key);
        assert!(Arc::ptr_eq(first, variant(&p, g, &a)));
        let second = variant(&p, g, &b);
        assert_eq!(second.formats, b);
        assert!(!Arc::ptr_eq(first, second));
        assert_ne!(second.key, first.key);
        assert!(Arc::ptr_eq(first, variant(&p, g, &a)));
        assert!(Arc::ptr_eq(second, variant(&p, g, &b)));
        assert_eq!(first.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
        assert_eq!(p.queue_state.load(Ordering::Relaxed), NOT_QUEUED);
    }

    #[test]
    fn pending_pipeline_is_recorded_for_the_next_launch() {
        let mut p = Pending::new(Vec::new(), None);
        p.key = "pending-pipeline-test".to_owned();
        // Simulate a worker that has taken the job but cannot finish it yet.
        p.queue_state.store(MAKING, Ordering::Relaxed);
        let p = Arc::new(p);

        assert!(!super::super::library::wanted_early(&p.key));
        // A compute pipeline is made at once when dispatches wait for theirs
        // (#1312, the default; it has no shaders here, so nothing comes of
        // it); either way it is noted for the next launch.
        assert!(ready(&p).is_none());
        assert!(super::super::library::wanted_early(&p.key));
    }

    #[test]
    fn draws_promote_early_pipelines_without_claiming_them_twice() {
        let earlier = Arc::new(Pending::new(Vec::new(), None));
        let promoted = Arc::new(Pending::new(Vec::new(), None));
        let requested = Arc::new(Pending::new(Vec::new(), None));
        let mut queue = Queue::new();

        assert_eq!(queue.push(&earlier, EARLY), Some(true));
        assert_eq!(queue.push(&promoted, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        assert_eq!(queue.push(&promoted, REQUESTED), Some(false));
        assert_eq!(queue.push(&promoted, REQUESTED), None);

        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &requested));
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop().unwrap(), &earlier));
        assert!(queue.pop().is_none());
    }
}
