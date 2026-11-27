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
            && let Some(left) = compute_wait_begin(frame)
        {
            let started = Instant::now();
            wait_compute(p, left);
            compute_wait_end(frame, started.elapsed());
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

/// The frame [`compute_wait_most`] is counted for and its waits so far.
static COMPUTE_SPENT: Mutex<ComputeBudget> = Mutex::new(ComputeBudget::new());

/// A frame's waits for compute pipelines: their time summed over the
/// title's threads, and while any thread waits, the moment all of the
/// frame's waits end.
///
/// Why the deadline: each wait was bounded by what the sum left when it
/// began, and a wait's time joined the sum only when it ended, so waits on
/// several recording threads at once each had the whole limit. Run 6 on the
/// owner's PC: frames 2413 and 2414 waited 12.8 and 13.1 s against 10.
/// Now a wait that begins while others wait ends with them at the latest.
struct ComputeBudget {
    frame: u64,
    spent: Duration,
    waiting: u32,
    deadline: Option<Instant>,
}

impl ComputeBudget {
    const fn new() -> Self {
        Self { frame: u64::MAX, spent: Duration::ZERO, waiting: 0, deadline: None }
    }

    /// A wait of frame `frame` begins at `now`: for at most the returned
    /// time, or `None` (not at all) past the limit `most`.
    fn begin(&mut self, frame: u64, most: Duration, now: Instant) -> Option<Duration> {
        if self.frame != frame {
            *self = Self { frame, ..Self::new() };
        }
        let mut left = most.saturating_sub(self.spent);
        if self.waiting > 0
            && let Some(deadline) = self.deadline
        {
            left = left.min(deadline.saturating_duration_since(now));
        }
        if left.is_zero() {
            return None;
        }
        self.waiting += 1;
        if self.waiting == 1 {
            self.deadline = Some(now + left);
        }
        Some(left)
    }

    /// A wait of frame `frame` that [`ComputeBudget::begin`] let begin took
    /// `took`; true the once it takes the frame to `most`.
    fn end(&mut self, frame: u64, took: Duration, most: Duration) -> bool {
        if self.frame != frame {
            return false;
        }
        self.waiting = self.waiting.saturating_sub(1);
        if self.waiting == 0 {
            self.deadline = None;
        }
        let before = self.spent;
        self.spent += took;
        before < most && self.spent >= most
    }
}

/// A wait of frame `frame` for a compute pipeline begins: for at most the
/// returned time ([`compute_wait_most`]; an hour without a limit), or not
/// at all (`None`).
fn compute_wait_begin(frame: u64) -> Option<Duration> {
    let Some(most) = compute_wait_most() else {
        return Some(Duration::from_secs(3600));
    };
    COMPUTE_SPENT.lock().unwrap_or_else(|e| e.into_inner()).begin(frame, most, Instant::now())
}

/// The wait [`compute_wait_begin`] let begin took `took`; said once a frame
/// when the frame's waits reach [`compute_wait_most`].
fn compute_wait_end(frame: u64, took: Duration) {
    let Some(most) = compute_wait_most() else {
        return;
    };
    let mut budget = COMPUTE_SPENT.lock().unwrap_or_else(|e| e.into_inner());
    if budget.end(frame, took, most) {
        tracing::warn!(
            frame,
            waited_ms = budget.spent.as_millis() as u64,
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
    } else if p.graphics.is_none() && spare_most() > 0 {
        COMPUTE_EARLY.fetch_add(1, Ordering::Relaxed);
        start(p, SPARE);
    }
}

/// Compute pipelines started at their creation that no earlier run used.
pub(crate) static COMPUTE_EARLY: AtomicU64 = AtomicU64::new(0);

/// How many workers at once may make compute pipelines no earlier run
/// used, started at their creation, not at their first dispatch: behind
/// everything requested and everything an earlier run used, on at most
/// this many workers (`VICTORY_PIPELINE_COMPUTE_EARLY`: a number, 0 at
/// first dispatch only; default 2).
///
/// Why: the title creates its compute pipelines while it loads and runs
/// many of them first in a scene's opening frames. Made only then, one
/// after another (run 4 on the owner's RTX 4070 / 5800X), the first
/// cutscene frames waited 13.6 and 14.6 s for four of them. Started at
/// creation on every worker (run 5, c8bdf90), ~2,900 were queued while
/// loading, most never used: the 12 workers compiled them for the whole
/// run at 1 to 8 s each (the driver; the CPU at 99 %), 1,014 were still
/// queued when it ended, and everything else on the CPU slowed with them.
/// On two workers they are made in the background over a few runs; the
/// made ones go to the pipeline library, whence later runs load them.
fn spare_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        let most = spare_most_of(std::env::var("VICTORY_PIPELINE_COMPUTE_EARLY").ok().as_deref());
        tracing::info!(most, "workers for compute pipelines started at their creation (VICTORY_PIPELINE_COMPUTE_EARLY)");
        most
    })
}

/// [`spare_most`] from the switch's value.
fn spare_most_of(v: Option<&str>) -> usize {
    match v.map(str::trim) {
        None | Some("") => 2,
        Some(v) => v.parse::<usize>().map_or(2, |n| n.min(64)),
    }
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
/// Queued as spare work: a compute pipeline no earlier run used
/// ([`spare_most`]).
const SPARE: u8 = 1;
const EARLY: u8 = 2;
const REQUESTED: u8 = 3;
const MAKING: u8 = 4;

struct Queue {
    spare: VecDeque<Arc<Pending>>,
    early: VecDeque<Arc<Pending>>,
    requested: VecDeque<Arc<Pending>>,
    /// Spare pipelines workers are making now.
    spare_busy: usize,
}

impl Queue {
    const fn new() -> Self {
        Self {
            spare: VecDeque::new(),
            early: VecDeque::new(),
            requested: VecDeque::new(),
            spare_busy: 0,
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
        match priority {
            REQUESTED => self.requested.push_back(p.clone()),
            EARLY => self.early.push_back(p.clone()),
            _ => self.spare.push_back(p.clone()),
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

    /// The next pipeline to make, and whether it is spare work (which the
    /// worker gives back with [`Queue::spare_done`]); spare work only while
    /// fewer than `spare_most` workers make it.
    fn pop(&mut self, spare_most: usize) -> Option<(Arc<Pending>, bool)> {
        // An early or spare entry can have been promoted and claimed already.
        let still = |state: u8| move |p: &Arc<Pending>| p.queue_state.load(Ordering::Relaxed) == state;
        let (p, spare) = if let Some(p) = self.requested.pop_front() {
            (p, false)
        } else if let Some(p) = std::iter::from_fn(|| self.early.pop_front()).find(still(EARLY)) {
            (p, false)
        } else if self.spare_busy < spare_most
            && let Some(p) = std::iter::from_fn(|| self.spare.pop_front()).find(still(SPARE))
        {
            self.spare_busy += 1;
            (p, true)
        } else {
            return None;
        };
        p.queue_state.store(MAKING, Ordering::Relaxed);
        Some((p, spare))
    }

    fn spare_done(&mut self) {
        self.spare_busy = self.spare_busy.saturating_sub(1);
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

fn next() -> (Arc<Pending>, bool) {
    let spare_most = spare_most();
    let mut queue = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if let Some(next) = queue.pop(spare_most) {
            return next;
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
                        let (p, spare) = next();
                        p.make();
                        if spare {
                            QUEUE.lock().unwrap_or_else(|e| e.into_inner()).spare_done();
                            // A worker waiting while the spare limit held.
                            AVAILABLE.notify_one();
                        }
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
    use super::ComputeBudget;
    use std::time::{Duration, Instant};

    /// A frame may make compute pipelines on the title's thread until it
    /// has spent the limit on them; the next frame may again.
    #[test]
    fn a_frame_stops_waiting_for_compute_pipelines_at_the_limit() {
        let most = Duration::from_secs(10);
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut b = ComputeBudget::new();
        assert_eq!(b.begin(7, most, t0), Some(most));
        assert!(!b.end(7, s(9), most));
        assert_eq!(b.begin(7, most, t0 + s(9)), Some(s(1)), "what is left of the limit");
        assert!(b.end(7, s(2), most), "said once, when the limit is reached");
        assert_eq!(b.begin(7, most, t0 + s(11)), None, "past the limit");
        assert_eq!(b.begin(8, most, t0 + s(12)), Some(most), "a new frame waits again");
        assert!(!b.end(8, Duration::from_millis(1), most));
        assert!(!b.end(7, s(5), most), "an old frame's wait counts for nothing");
        assert_eq!(b.begin(8, most, t0 + s(13)), Some(most - Duration::from_millis(1)));
    }

    #[test]
    fn waits_at_once_on_several_threads_end_together() {
        let most = Duration::from_secs(10);
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut b = ComputeBudget::new();
        // Run 6: a second thread began 3 s into the first one's wait and
        // was given the whole limit; now it ends with the first.
        assert_eq!(b.begin(1, most, t0), Some(s(10)));
        assert_eq!(b.begin(1, most, t0 + s(3)), Some(s(7)));
        assert!(!b.end(1, s(4), most));
        assert_eq!(b.begin(1, most, t0 + s(9)), Some(s(1)), "the first still waits: its deadline holds");
        assert!(b.end(1, s(6), most));
        assert!(!b.end(1, s(1), most));
        // All done: the summed time is the limit, and no deadline is left.
        assert_eq!(b.begin(1, most, t0 + s(20)), None);
        assert_eq!(b.waiting, 0);
        // A wait alone, after others ended, is bounded by the sum only.
        let mut b = ComputeBudget::new();
        assert_eq!(b.begin(2, most, t0), Some(s(10)));
        assert!(!b.end(2, s(1), most));
        assert_eq!(b.begin(2, most, t0 + s(30)), Some(s(9)), "the old deadline is gone");
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
        let (taken, _) = queue.pop(0).expect("one queued");
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

        assert!(Arc::ptr_eq(&queue.pop(0).unwrap().0, &requested));
        assert!(Arc::ptr_eq(&queue.pop(0).unwrap().0, &promoted));
        assert_eq!(queue.push(&promoted, REQUESTED), None);
        assert!(Arc::ptr_eq(&queue.pop(0).unwrap().0, &earlier));
        assert!(queue.pop(0).is_none());
    }

    #[test]
    fn spare_pipelines_go_last_on_at_most_so_many_workers() {
        let [s1, s2, s3, early, requested] = [(); 5].map(|()| Arc::new(Pending::new(Vec::new(), None)));
        let mut queue = Queue::new();
        for s in [&s1, &s2, &s3] {
            assert_eq!(queue.push(s, SPARE), Some(true));
        }
        assert_eq!(queue.push(&early, EARLY), Some(true));
        assert_eq!(queue.push(&requested, REQUESTED), Some(true));
        let take = |queue: &mut Queue| queue.pop(2).map(|(p, spare)| (Arc::as_ptr(&p), spare));
        assert_eq!(take(&mut queue), Some((Arc::as_ptr(&requested), false)));
        assert_eq!(take(&mut queue), Some((Arc::as_ptr(&early), false)));
        assert_eq!(take(&mut queue), Some((Arc::as_ptr(&s1), true)));
        assert_eq!(take(&mut queue), Some((Arc::as_ptr(&s2), true)));
        assert_eq!(take(&mut queue), None, "two spare ones at most at once");
        queue.spare_done();
        assert_eq!(take(&mut queue), Some((Arc::as_ptr(&s3), true)));
        // A spare one asked for by a dispatch is requested work, once.
        let s4 = Arc::new(Pending::new(Vec::new(), None));
        assert_eq!(queue.push(&s4, SPARE), Some(true));
        assert_eq!(queue.push(&s4, REQUESTED), Some(false));
        assert_eq!(take(&mut queue), Some((Arc::as_ptr(&s4), false)));
        queue.spare_done();
        queue.spare_done();
        assert_eq!(take(&mut queue), None, "its spare entry is not made again");
        assert!(queue.spare.is_empty());
    }

    #[test]
    fn the_spare_worker_count_reads_its_switch() {
        assert_eq!(spare_most_of(None), 2);
        assert_eq!(spare_most_of(Some("0")), 0);
        assert_eq!(spare_most_of(Some(" 4 ")), 4);
        assert_eq!(spare_most_of(Some("on")), 2);
    }
}
