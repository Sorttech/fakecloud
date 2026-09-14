//! Minimal JavaScript runtime backing CloudFront Functions and
//! ConnectionFunctions. We embed `boa_engine` to actually execute the
//! user's `function handler(event) { ... }` against a caller-provided
//! event object, mirroring the real AWS shape:
//!
//! - decode the function source, eval it in a fresh `Context`;
//! - parse the event JSON into a JS value (`(<json>)`);
//! - call `handler(event)`;
//! - JSON.stringify the return value;
//! - capture `console.log/error` output as execution log lines.
//!
//! Limits are enforced in three layers:
//!
//! 1. boa's loop iteration + recursion caps trip on hot loops so the
//!    interpreter eventually returns control even under adversarial
//!    user JS.
//! 2. A compute budget: real CloudFront Functions are bounded by the CPU
//!    a request consumes (~1ms, and 2MB of memory), not by elapsed time.
//!    The execution runs on a dedicated OS thread that measures its own
//!    CPU time, and a run that used more than [`EXECUTION_TIMEOUT`] of it
//!    fails with the time-limit error. Measuring CPU rather than wall
//!    time means a host that descheduled the thread -- a loaded CI runner
//!    -- does not fail a handler that did almost no work.
//! 3. A wall-clock safety net: the calling thread waits on a
//!    `mpsc::sync_channel` via `recv_timeout` for [`WALL_CLOCK_LIMIT`]. If
//!    the JS still hasn't finished we abandon the worker thread
//!    (best-effort -- boa's iteration limit will eventually let it die)
//!    and return the same time-limit error.
//!
//! The 250ms budget is looser than AWS's so ordinary handlers never trip
//! it in a debug build, while `while(1){}` is still stopped in tests.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use boa_engine::object::ObjectInitializer;
use boa_engine::property::Attribute;
use boa_engine::{js_string, Context, JsValue, NativeFunction, Source};

/// Compute budget for a single TestFunction / TestConnectionFunction
/// invocation, measured as CPU time on the executing thread. AWS bounds
/// production traffic at ~1ms of CPU; 250ms leaves ordinary handlers far
/// below it even unoptimized, while a CPU-bound handler still exceeds it.
pub(crate) const EXECUTION_TIMEOUT: Duration = Duration::from_millis(250);

/// How long the caller waits for the worker thread before abandoning it.
/// A safety net for a run that never returns; the compute budget above is
/// the limit a handler is actually held to, so this is generous enough
/// that a descheduled thread on a loaded host still reports back.
const WALL_CLOCK_LIMIT: Duration = Duration::from_secs(5);

/// How often the caller checks the worker's CPU time against the budget.
const WATCHDOG_POLL: Duration = Duration::from_millis(5);

/// boa loop iteration cap. Tight enough that `while(1){}` exits the VM
/// well within the wall-clock budget on any reasonable host, loose
/// enough that small loops in real handlers run to completion.
const LOOP_ITERATION_LIMIT: u64 = 200_000;
const RECURSION_LIMIT: usize = 1_000;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Result of executing a CloudFront-style function. Either `output`
/// (the JSON-encoded handler return value) or `error` will be set;
/// `logs` is always populated (possibly empty) with whatever the user
/// JS wrote to `console.log` / `console.error`. On error we also push
/// a synthetic log line so callers that surface logs alone still see
/// the failure.
#[derive(Debug, Clone, Default)]
pub(crate) struct JsExecution {
    pub output: Option<String>,
    pub error: Option<String>,
    pub logs: Vec<String>,
    /// Synthetic compute utilisation in percent. Real CloudFront
    /// returns a number 0..=100 representing the share of the per-
    /// request CPU budget consumed. We approximate it by linear
    /// interpolation against `EXECUTION_TIMEOUT` and saturate at 100,
    /// then deliberately flip past 100 on errors / timeouts so callers
    /// can detect failure from the metric alone.
    pub compute_utilization: u32,
}

/// Run `handler(event)` defined in `code` against `event_json` on a
/// dedicated worker thread, holding it to the `EXECUTION_TIMEOUT` compute
/// budget.
pub(crate) fn run_handler(code: &str, event_json: &[u8]) -> JsExecution {
    run_handler_with_limits(code, event_json, EXECUTION_TIMEOUT, WALL_CLOCK_LIMIT, || {})
}

/// `before_start` runs on the worker thread before its clock starts; tests
/// use it to stand in for a thread the host is slow to schedule.
fn run_handler_with_limits(
    code: &str,
    event_json: &[u8],
    compute_budget: Duration,
    wall_limit: Duration,
    before_start: fn(),
) -> JsExecution {
    let code = code.to_owned();
    let event = event_json.to_vec();
    let (tx, rx) = mpsc::sync_channel::<WorkerMessage>(2);

    // Each call gets its own thread because boa's `Context` holds
    // `Rc`s and is `!Send`. We can't pre-spawn a worker pool without
    // marshalling the script + event via channels anyway, so a fresh
    // thread per call is the simpler shape.
    let spawned = std::thread::Builder::new()
        .name("cloudfront-js".to_string())
        // Boa's bytecode VM is recursive so we want a generous stack.
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            before_start();
            let clock = ComputeClock::start();
            let _ = tx.send(WorkerMessage::Started(clock.cpu));
            let mut result = run_handler_blocking(&code, &event, &clock);
            // Covers a run that crossed the budget between the caller's polls.
            if clock.elapsed() > compute_budget {
                result = time_limit_exceeded(result.logs);
            }
            // If the receiver has timed out and gone away the send
            // simply errors; we don't care — the worker is being
            // abandoned.
            let _ = tx.send(WorkerMessage::Done(result));
        });
    if spawned.is_err() {
        return worker_failed();
    }

    // Watch the worker: stop waiting as soon as its CPU time passes the
    // budget, so a CPU-bound handler is cut off at the budget rather than
    // holding the caller until the wall-clock safety net. Until the worker
    // reports in, nothing counts against the budget -- a thread the host is
    // slow to schedule has used none -- and only the safety net applies.
    // Without a thread CPU clock on this platform, time elapsed since the
    // worker started stands in for it.
    let wall_start = Instant::now();
    let mut watch = Watch::NotStarted;
    loop {
        let remaining = wall_limit.saturating_sub(wall_start.elapsed());
        match rx.recv_timeout(remaining.min(WATCHDOG_POLL)) {
            Ok(WorkerMessage::Started(cpu)) => {
                watch = match cpu {
                    Some((clock, start)) => Watch::Cpu(clock, start),
                    None => Watch::Elapsed(Instant::now()),
                };
            }
            Ok(WorkerMessage::Done(mut exec)) => {
                // Floor compute_utilization at 1% on success so callers
                // don't mistake a successful run for an unrun one.
                if exec.error.is_none() && exec.compute_utilization == 0 {
                    exec.compute_utilization = 1;
                }
                return exec;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return worker_failed(),
        }
        if wall_start.elapsed() >= wall_limit {
            return time_limit_exceeded(Vec::new());
        }
        let used = match watch {
            Watch::NotStarted => None,
            // A read fails once the thread has exited; its result is
            // already on the way, so keep waiting for it.
            Watch::Cpu(clock, start) => clock.read().map(|now| now.saturating_sub(start)),
            Watch::Elapsed(started) => Some(started.elapsed()),
        };
        if used.is_some_and(|u| u > compute_budget) {
            return time_limit_exceeded(Vec::new());
        }
    }
}

/// What the worker thread reports: first its CPU clock and the reading it
/// started from (so the caller measures the run from the same point as the
/// worker's own check), then the result.
enum WorkerMessage {
    Started(Option<(ThreadCpuClock, Duration)>),
    Done(JsExecution),
}

/// How the caller measures the worker's compute while waiting on it.
enum Watch {
    /// The worker has not reported in yet; nothing counts against the budget.
    NotStarted,
    /// The worker's CPU clock and its reading when the run started.
    Cpu(ThreadCpuClock, Duration),
    /// No thread CPU clock on this platform: time since the worker started.
    Elapsed(Instant),
}

/// The worker thread either failed to spawn or panicked partway through.
/// Surfaced distinctly so a host-level problem doesn't get misdiagnosed as
/// adversarial JS.
fn worker_failed() -> JsExecution {
    let msg = "function execution worker thread panicked or failed to spawn".to_string();
    JsExecution {
        output: None,
        error: Some(msg.clone()),
        logs: vec![format!("ERROR: {msg}")],
        compute_utilization: 101,
    }
}

/// The error a run over its compute budget (or past the wall-clock safety
/// net) reports, keeping whatever it logged before the limit.
fn time_limit_exceeded(mut logs: Vec<String>) -> JsExecution {
    let msg = format!(
        "function execution exceeded the {}ms time limit",
        EXECUTION_TIMEOUT.as_millis()
    );
    logs.push(format!("ERROR: {msg}"));
    JsExecution {
        output: None,
        error: Some(msg),
        logs,
        compute_utilization: 101,
    }
}

/// Measures the compute a run consumed: CPU time on the current thread
/// where the platform reports it, elapsed time otherwise.
struct ComputeClock {
    cpu: Option<(ThreadCpuClock, Duration)>,
    wall_start: Instant,
}

impl ComputeClock {
    fn start() -> Self {
        Self {
            cpu: ThreadCpuClock::current().and_then(|c| c.read().map(|start| (c, start))),
            wall_start: Instant::now(),
        }
    }

    fn elapsed(&self) -> Duration {
        match self
            .cpu
            .and_then(|(c, start)| c.read().map(|now| (now, start)))
        {
            Some((now, start)) => now.saturating_sub(start),
            None => self.wall_start.elapsed(),
        }
    }
}

/// A handle to one thread's CPU-time clock that any thread can read, so the
/// caller can watch the worker while it runs. Linux exposes a per-thread
/// clock id; macOS exposes the thread's Mach port. Elsewhere there is none
/// and callers fall back to elapsed time.
#[derive(Clone, Copy)]
struct ThreadCpuClock {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    clock_id: libc::clockid_t,
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    thread: libc::mach_port_t,
}

impl ThreadCpuClock {
    /// The calling thread's clock.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn current() -> Option<Self> {
        let mut clock_id: libc::clockid_t = 0;
        // SAFETY: `clock_id` is a valid out-pointer, and `pthread_self()` is
        // always a live thread (the caller).
        let rc = unsafe { libc::pthread_getcpuclockid(libc::pthread_self(), &mut clock_id) };
        (rc == 0).then_some(Self { clock_id })
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn current() -> Option<Self> {
        // SAFETY: `pthread_self()` is always a live thread; the returned port
        // is borrowed from the pthread (no reference to release).
        let thread = unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) };
        (thread != 0).then_some(Self { thread })
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    fn current() -> Option<Self> {
        None
    }

    /// CPU time the thread has consumed, or `None` once it has exited.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn read(self) -> Option<Duration> {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `ts` is a valid, writable timespec; a stale clock id (the
        // thread exited) is reported through the return code.
        let rc = unsafe { libc::clock_gettime(self.clock_id, &mut ts) };
        (rc == 0).then(|| Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32))
    }

    #[cfg(any(target_os = "macos", target_os = "ios"))]
    fn read(self) -> Option<Duration> {
        // SAFETY: an all-zero `thread_basic_info` is a valid value for this
        // plain-integer struct.
        let mut info: libc::thread_basic_info = unsafe { std::mem::zeroed() };
        let mut count = libc::THREAD_BASIC_INFO_COUNT;
        // SAFETY: `info` is large enough for THREAD_BASIC_INFO_COUNT
        // integers, which `count` declares; a dead thread's port is reported
        // through the return code.
        let rc = unsafe {
            libc::thread_info(
                self.thread,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                &mut info as *mut libc::thread_basic_info as libc::thread_info_t,
                &mut count,
            )
        };
        if rc != libc::KERN_SUCCESS {
            return None;
        }
        let micros = |t: libc::time_value_t| t.seconds as u64 * 1_000_000 + t.microseconds as u64;
        Some(Duration::from_micros(
            micros(info.user_time) + micros(info.system_time),
        ))
    }

    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    )))]
    fn read(self) -> Option<Duration> {
        None
    }
}

fn run_handler_blocking(code: &str, event_json: &[u8], clock: &ComputeClock) -> JsExecution {
    let logs: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
    let mut ctx = Context::default();
    ctx.runtime_limits_mut()
        .set_loop_iteration_limit(LOOP_ITERATION_LIMIT);
    ctx.runtime_limits_mut()
        .set_recursion_limit(RECURSION_LIMIT);

    if let Err(err) = install_console(&mut ctx, &logs) {
        return error_execution(format!("failed to install console: {err}"), &logs, clock);
    }

    if let Err(err) = ctx.eval(Source::from_bytes(code.as_bytes())) {
        return error_execution(format!("{}", err), &logs, clock);
    }

    let event_str = match std::str::from_utf8(event_json) {
        Ok(s) => s,
        Err(_) => {
            return error_execution("EventObject is not valid UTF-8".to_string(), &logs, clock);
        }
    };
    // Wrap in parens so a top-level `{ ... }` object literal parses as
    // an expression rather than a block statement.
    let event_src = format!("({})", event_str);
    let event = match ctx.eval(Source::from_bytes(event_src.as_bytes())) {
        Ok(v) => v,
        Err(err) => {
            return error_execution(format!("invalid EventObject JSON: {err}"), &logs, clock);
        }
    };

    let handler = match ctx.global_object().get(js_string!("handler"), &mut ctx) {
        Ok(h) => h,
        Err(err) => {
            return error_execution(
                format!("function handler is not defined: {err}"),
                &logs,
                clock,
            );
        }
    };
    let Some(handler_fn) = handler.as_callable() else {
        return error_execution("function handler is not callable".to_string(), &logs, clock);
    };

    let returned = match handler_fn.call(&JsValue::undefined(), &[event], &mut ctx) {
        Ok(v) => v,
        Err(err) => {
            return error_execution(format!("{}", err), &logs, clock);
        }
    };

    let stringified = match stringify(&mut ctx, returned) {
        Ok(s) => s,
        Err(err) => {
            return error_execution(
                format!("failed to JSON.stringify result: {err}"),
                &logs,
                clock,
            );
        }
    };

    if stringified.len() > MAX_OUTPUT_BYTES {
        return error_execution(
            format!("function output exceeded {MAX_OUTPUT_BYTES} bytes"),
            &logs,
            clock,
        );
    }

    let captured = logs.borrow().clone();
    JsExecution {
        output: Some(stringified),
        error: None,
        logs: captured,
        compute_utilization: utilization_pct(clock.elapsed()),
    }
}

fn error_execution(
    msg: String,
    logs: &Rc<RefCell<Vec<String>>>,
    clock: &ComputeClock,
) -> JsExecution {
    let mut captured = logs.borrow().clone();
    captured.push(format!("ERROR: {msg}"));
    // Saturate past 100 on any failure so the metric alone signals the
    // run did not complete cleanly, regardless of how fast it failed.
    let elapsed_pct = utilization_pct(clock.elapsed());
    let pct = elapsed_pct.max(101);
    JsExecution {
        output: None,
        error: Some(msg),
        logs: captured,
        compute_utilization: pct,
    }
}

fn utilization_pct(elapsed: Duration) -> u32 {
    let limit_us = EXECUTION_TIMEOUT.as_micros().max(1);
    let used_us = elapsed.as_micros();
    let pct = (used_us * 100) / limit_us;
    if pct > 100 {
        100
    } else {
        pct as u32
    }
}

fn install_console(
    ctx: &mut Context,
    logs: &Rc<RefCell<Vec<String>>>,
) -> Result<(), boa_engine::JsError> {
    let logs_log = Rc::clone(logs);
    let log_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let mut parts: Vec<String> = Vec::with_capacity(args.len());
            for a in args {
                let s = a
                    .to_string(ctx)
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                parts.push(s);
            }
            logs_log.borrow_mut().push(parts.join(" "));
            Ok(JsValue::undefined())
        })
    };
    let logs_err = Rc::clone(logs);
    let err_fn = unsafe {
        NativeFunction::from_closure(move |_this, args, ctx| {
            let mut parts: Vec<String> = Vec::with_capacity(args.len());
            for a in args {
                let s = a
                    .to_string(ctx)
                    .map(|s| s.to_std_string_escaped())
                    .unwrap_or_default();
                parts.push(s);
            }
            logs_err.borrow_mut().push(parts.join(" "));
            Ok(JsValue::undefined())
        })
    };

    let console = ObjectInitializer::new(ctx)
        .function(log_fn, js_string!("log"), 0)
        .function(err_fn, js_string!("error"), 0)
        .build();
    ctx.register_global_property(js_string!("console"), console, Attribute::all())?;
    Ok(())
}

fn stringify(ctx: &mut Context, value: JsValue) -> Result<String, boa_engine::JsError> {
    let stringify = ctx.eval(Source::from_bytes(b"JSON.stringify"))?;
    let Some(stringify_fn) = stringify.as_callable() else {
        return Err(boa_engine::JsNativeError::typ()
            .with_message("JSON.stringify missing")
            .into());
    };
    let result = stringify_fn.call(&JsValue::undefined(), &[value], ctx)?;
    if result.is_undefined() {
        // JSON.stringify(undefined) yields undefined; treat as empty.
        return Ok(String::new());
    }
    Ok(result.to_string(ctx)?.to_std_string_escaped())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_modified_event_as_json() {
        let exec = run_handler(
            r#"function handler(e) { e.x = "y"; return e; }"#,
            br#"{"headers":{}}"#,
        );
        assert!(exec.error.is_none(), "unexpected error: {:?}", exec.error);
        let out = exec.output.expect("output");
        assert!(out.contains("\"x\":\"y\""), "got {out}");
        assert!(
            exec.compute_utilization <= 100,
            "got {}",
            exec.compute_utilization
        );
    }

    #[test]
    fn modifies_request_headers_aws_shape() {
        // Mirrors the real CloudFront Functions request shape so
        // callers can validate header rewrites in tests.
        let exec = run_handler(
            r#"function handler(event) {
                event.request.headers["x-foo"] = {value: "bar"};
                return event.request;
            }"#,
            br#"{"version":"1.0","context":{},"viewer":{},"request":{"method":"GET","uri":"/","querystring":{},"headers":{},"cookies":{}}}"#,
        );
        assert!(exec.error.is_none(), "unexpected error: {:?}", exec.error);
        let out = exec.output.expect("output");
        assert!(out.contains("\"x-foo\""), "got {out}");
        assert!(out.contains("\"bar\""), "got {out}");
    }

    #[test]
    fn surfaces_thrown_error() {
        let exec = run_handler(r#"function handler() { throw new Error("boom"); }"#, b"{}");
        assert!(exec.output.is_none());
        let err = exec.error.expect("error");
        assert!(err.contains("boom"), "got {err}");
        assert!(
            exec.logs.iter().any(|l| l.contains("boom")),
            "expected error in logs, got {:?}",
            exec.logs
        );
        assert!(
            exec.compute_utilization > 100,
            "expected >100 on error, got {}",
            exec.compute_utilization
        );
    }

    #[test]
    fn captures_console_log() {
        let exec = run_handler(
            r#"function handler(e) { console.log("a", "b"); return e; }"#,
            b"{}",
        );
        assert!(exec.error.is_none());
        assert!(exec.logs.iter().any(|l| l == "a b"));
    }

    #[test]
    fn errors_when_handler_missing() {
        let exec = run_handler("var x = 1;", b"{}");
        assert!(exec.error.is_some());
        assert!(
            exec.compute_utilization > 100,
            "expected >100 on error, got {}",
            exec.compute_utilization
        );
    }

    #[test]
    fn errors_when_event_is_invalid_json() {
        let exec = run_handler(r#"function handler(e) { return e; }"#, b"not-json");
        assert!(exec.error.is_some());
    }

    #[test]
    fn a_run_over_its_compute_budget_reports_the_time_limit() {
        // A zero budget is exceeded by any run: either the caller's watchdog
        // or the worker's own final check reports it, whichever sees it first.
        let exec = run_handler_with_limits(
            r#"function handler(e) { return e; }"#,
            b"{}",
            Duration::ZERO,
            WALL_CLOCK_LIMIT,
            || {},
        );
        assert!(exec.output.is_none(), "got output {:?}", exec.output);
        let err = exec.error.expect("error");
        assert!(err.contains("250ms time limit"), "got {err}");
        assert!(exec.compute_utilization > 100);
        assert!(exec.logs.iter().any(|l| l.starts_with("ERROR: ")));
    }

    #[test]
    fn a_worker_that_does_not_report_back_hits_the_wall_clock_safety_net() {
        let exec = run_handler_with_limits(
            r#"function handler(e) { return e; }"#,
            b"{}",
            EXECUTION_TIMEOUT,
            Duration::ZERO,
            || {},
        );
        let err = exec.error.expect("error");
        assert!(err.contains("time limit"), "got {err}");
        assert!(exec.compute_utilization > 100);
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    #[test]
    fn a_worker_slow_to_start_is_not_charged_for_the_wait() {
        // The host took longer than the whole budget to get the worker going;
        // the handler itself is trivial and must still succeed.
        let exec = run_handler_with_limits(
            r#"function handler(e) { return e; }"#,
            b"{}",
            EXECUTION_TIMEOUT,
            WALL_CLOCK_LIMIT,
            || std::thread::sleep(EXECUTION_TIMEOUT + Duration::from_millis(150)),
        );
        assert!(exec.error.is_none(), "unexpected error: {:?}", exec.error);
        assert_eq!(exec.output.as_deref(), Some("{}"));
    }

    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios"
    ))]
    #[test]
    fn a_descheduled_thread_accrues_no_compute() {
        // The budget is CPU time: a thread that is not running -- here
        // sleeping, on a loaded host descheduled -- does not use it up.
        let clock = ComputeClock::start();
        std::thread::sleep(EXECUTION_TIMEOUT + Duration::from_millis(100));
        assert!(
            clock.elapsed() < EXECUTION_TIMEOUT,
            "sleeping consumed {:?} of compute",
            clock.elapsed()
        );
    }

    #[test]
    fn busy_work_accrues_compute() {
        // Bounded by work done, not elapsed time, so a loaded host that
        // deschedules this thread only makes it take longer.
        let clock = ComputeClock::start();
        let mut x: u64 = 0;
        for i in 0..50_000_000u64 {
            x = std::hint::black_box(x.wrapping_add(i));
        }
        std::hint::black_box(x);
        assert!(
            clock.elapsed() >= Duration::from_millis(1),
            "the work measured {:?}",
            clock.elapsed()
        );
    }

    #[test]
    fn a_cpu_bound_handler_is_cut_off_at_its_budget() {
        // Catastrophic regex backtracking burns CPU inside one builtin call,
        // so boa's loop and recursion caps never trip; only the compute
        // budget can stop it. It runs for seconds (about 6s unoptimized,
        // doubling per extra `a`), well past half the safety net, so the
        // caller returning quickly proves the watchdog cut it off. The
        // abandoned worker then finishes in the background.
        let started = Instant::now();
        let exec = run_handler(
            &format!(
                r#"function handler() {{ return /^(a+)+$/.test("{}b"); }}"#,
                "a".repeat(24)
            ),
            b"{}",
        );
        let took = started.elapsed();
        let err = exec.error.expect("error");
        assert!(err.contains("time limit"), "got {err}");
        assert!(
            took < WALL_CLOCK_LIMIT / 2,
            "caller waited {took:?}; the budget should stop it well before the safety net"
        );
    }

    #[test]
    fn infinite_loop_is_killed_by_timeout() {
        let exec = run_handler(r#"function handler() { while(1){} }"#, b"{}");
        assert!(exec.output.is_none());
        let err = exec.error.expect("error");
        // Either the wall-clock recv_timeout fired or boa's iteration
        // cap tripped — both are acceptable kill signals; in either
        // case ComputeUtilization saturates past 100 and an error log
        // is present.
        assert!(
            err.contains("time limit") || err.contains("limit") || err.contains("iteration"),
            "expected timeout/iteration error, got {err}"
        );
        assert!(
            exec.compute_utilization > 100,
            "expected >100 after timeout, got {}",
            exec.compute_utilization
        );
        assert!(
            !exec.logs.is_empty(),
            "expected error log line, got empty logs"
        );
    }
}
