//! Native CPU samples of an iOS (tvOS, watchOS, visionOS) simulator app on macOS (attribution only,
//! SPEC rule 3) through Apple's Time Profiler (`xcrun xctrace`), the sampler Xcode uses for such apps.
//!
//! Why not `/usr/bin/sample`, as for a macOS app: a simulator app's code runs on the simulator's own
//! system (its dyld shared cache, SwiftUI, UIKit), and Time Profiler is the one sampler that both
//! reaches it without root and says when: a sample is a thread found running on a core every 1 ms by
//! the kernel (kperf), with its time, so samples split by phase and waiting threads are never counted.
//! Its backtraces of Swift concurrency are the task's logical chain (resumed async functions under
//! their callers) up to the task's entry, not the executor's thread.
//!
//! What it costs: `xctrace` and its recording service take about 300 MB and tens of seconds to start
//! and to save, outside the app (it runs in attribution blocks only). It attaches to one process: the
//! simulator app's, through the simulator device that runs it (`--device <udid>`).

use crate::etw::{Deep, Module};
use crate::model::{Error, Result};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const INTERVAL_US: f64 = 1000.0;

pub struct Capture {
    pid: u32,
    child: Child,
    trace: PathBuf,
    /// Host clock when xctrace said it was recording.
    from_ns: u64,
}

/// The simulator device a simulator app's executable runs in (`…/CoreSimulator/Devices/<udid>/…`).
pub fn device_of(exe: &Path) -> Option<String> {
    let s = exe.to_string_lossy();
    let rest = &s[s.find("/CoreSimulator/Devices/")? + 23..];
    rest.split('/').next().filter(|u| u.len() == 36).map(str::to_string)
}

impl Capture {
    /// Starts Time Profiler on `pid` and returns once it records (xctrace takes seconds to attach).
    /// The first attach on a Mac whose recording service has not run yet can answer that the process
    /// does not exist; one more attempt, a few seconds later, finds it.
    pub fn start(pid: u32, device: Option<&str>) -> Result<Capture> {
        match Self::attach(pid, device) {
            Err(e) if e.message.contains("Cannot find process") => {
                std::thread::sleep(Duration::from_secs(3));
                Self::attach(pid, device).map_err(|again| Error::msg(format!("{} (and again after 3 s: {})", e.message, again.message)))
            }
            other => other,
        }
    }

    fn attach(pid: u32, device: Option<&str>) -> Result<Capture> {
        let trace = crate::host::paths::run().join(format!("xctrace-{}-{pid}.trace", std::process::id()));
        let _ = std::fs::remove_dir_all(&trace);
        let mut c = Command::new("xcrun");
        c.args(["xctrace", "record", "--template", "Time Profiler", "--attach", &pid.to_string(), "--no-prompt", "--output"]).arg(&trace);
        if let Some(d) = device {
            c.args(["--device", d]);
        }
        let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| Error::msg(format!("start xctrace: {e}")))?;
        let out = child.stdout.take().ok_or_else(|| Error::msg("xctrace has no output"))?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for l in BufReader::new(out).lines().map_while(std::result::Result::ok) {
                let _ = tx.send(l);
            }
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            // "Ctrl-C to stop the recording": attached and sampling.
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(l) if l.contains("Ctrl-C to stop") => return Ok(Capture { pid, child, trace, from_ns: crate::host::now_ns() }),
                Ok(_) => continue,
                Err(_) => {}
            }
            if child.try_wait().ok().flatten().is_some() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = std::io::Read::read_to_string(&mut e, &mut err);
                }
                return Err(Error::msg(format!("xctrace could not record process {pid}: {}", err.trim())));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::msg("xctrace did not start recording in 120 s"));
            }
        }
    }

    pub fn stop(mut self) -> Result<Deep> {
        let to_ns = crate::host::now_ns();
        unsafe { libc::kill(self.child.id() as i32, libc::SIGINT) };
        let _ = self.child.wait();
        let export = |args: &[&str]| -> Option<String> {
            let o = Command::new("xcrun").args(["xctrace", "export", "--input"]).arg(&self.trace).args(args).stdin(Stdio::null()).stderr(Stdio::null()).output().ok()?;
            o.status.success().then(|| String::from_utf8_lossy(&o.stdout).to_string())
        };
        let toc = export(&["--toc"]);
        let table = export(&["--xpath", r#"/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]"#]);
        let _ = std::fs::remove_dir_all(&self.trace);
        let (Some(toc), Some(table)) = (toc, table) else { return Err(Error::msg("xctrace saved no trace to export")) };
        // Sample times count from the trace's start, which the table of contents gives as a wall-clock
        // date: the host clock at that moment is now's, less the time since.
        let start_ms = between(&toc, "<start-date>", "</start-date>").and_then(iso_epoch_ms).ok_or_else(|| Error::msg("xctrace's trace has no start date"))?;
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64() * 1000.0).unwrap_or(0.0);
        let t0 = crate::host::now_ns().saturating_sub(((now_ms - start_ms).max(0.0) * 1e6) as u64);
        let mut deep = parse(&table, t0);
        deep.pids = vec![self.pid];
        // The trace starts before xctrace says it records and ends after SIGINT reaches it (a second
        // or more on each side): only samples inside the capture strobe asked for are kept.
        let before = deep.samples.len();
        deep.samples.retain(|s| s.0 >= self.from_ns && s.0 <= to_ns);
        if before > deep.samples.len() {
            deep.dropped.insert("outside the capture (xctrace starting or saving)".into(), (before - deep.samples.len()) as u64);
        }
        (deep.t0_ns, deep.t1_ns) = (self.from_ns, to_ns);
        if deep.samples.is_empty() {
            return Err(Error::msg("Time Profiler recorded no samples of the process (idle, or its process ended)"));
        }
        Ok(deep)
    }
}

/// The text between two markers.
fn between<'a>(s: &'a str, a: &str, b: &str) -> Option<&'a str> {
    let i = s.find(a)? + a.len();
    Some(&s[i..i + s[i..].find(b)?])
}

/// `2026-11-04T14:57:34.319+11:00` as Unix epoch ms.
fn iso_epoch_ms(s: &str) -> Option<f64> {
    let (date, time) = s.trim().split_once('T')?;
    let mut d = date.split('-').map(|x| x.parse::<i64>());
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let (clock, offset_min) = match time.find(['+', '-', 'Z']) {
        Some(i) if &time[i..] == "Z" => (&time[..i], 0i64),
        Some(i) => {
            let sign = if time.as_bytes()[i] == b'-' { -1 } else { 1 };
            let (h, mm) = time[i + 1..].split_once(':').unwrap_or((&time[i + 1..], "0"));
            (&time[..i], sign * (h.parse::<i64>().ok()? * 60 + mm.parse::<i64>().ok()?))
        }
        None => (time, 0),
    };
    let mut c = clock.split(':');
    let (h, mi, sec) = (c.next()?.parse::<i64>().ok()?, c.next()?.parse::<i64>().ok()?, c.next()?.parse::<f64>().ok()?);
    // Days from the civil date (Howard Hinnant's algorithm).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let days = era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 - offset_min * 60) as f64 + sec) * 1000.0)
}

fn attr(attrs: &str, key: &str) -> Option<String> {
    let k = format!(" {key}=\"");
    let i = attrs.find(&k)? + k.len();
    Some(unescape(&attrs[i..i + attrs[i..].find('"')?]))
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn hex(s: &str) -> Option<u64> {
    u64::from_str_radix(s.trim_start_matches("0x"), 16).ok()
}

#[derive(Default, Clone)]
struct Frame {
    addr: u64,
    name: String,
    binary: Option<u64>,
    file: Option<u64>,
    line: u32,
}

/// The `time-profile` table: one `<row>` per sample, each value written in full the first time
/// (`id="N"`) and referred to after (`ref="N"`), ids shared by every element type. Frames are leaf
/// first; a frame carries its binary (name, path, load address) and, with debug info, its source.
fn parse(xml: &str, t0: u64) -> Deep {
    let mut deep = Deep { interval_us: INTERVAL_US, t0_ns: t0, ..Deep::default() };
    let (mut threads, mut procs): (HashMap<u64, (u32, u32)>, HashMap<u64, u32>) = (HashMap::new(), HashMap::new());
    let mut frames: HashMap<u64, Frame> = HashMap::new();
    let mut binaries: HashMap<u64, (String, String, u64)> = HashMap::new();
    let mut paths: HashMap<u64, String> = HashMap::new();
    let mut backtraces: HashMap<u64, Vec<u64>> = HashMap::new();
    let mut index: HashMap<Vec<u64>, u32> = HashMap::new();
    let mut outer: HashMap<u64, u64> = HashMap::new();
    // The row being read: time, thread id, its frames; the thread, process, frame and backtrace
    // being defined, and the element whose text is being read.
    let (mut time, mut thread, mut stack): (u64, Option<u64>, Vec<u64>) = (0, None, Vec::new());
    let mut def_thread: Option<(u64, String)> = None;
    let mut def_frame: Option<(u64, Frame)> = None;
    let (mut def_proc, mut def_bt): (Option<u64>, Option<u64>) = (None, None);
    let (mut tid, mut pid) = (0u32, 0u32);
    let mut text_of: Option<(&str, Option<u64>)> = None;
    let mut t1 = t0;
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        let text = &rest[..lt];
        if let Some((el, id)) = text_of.take() {
            match el {
                "sample-time" => time = text.trim().parse().unwrap_or(0),
                "tid" => tid = text.trim().parse().unwrap_or(0),
                "pid" => pid = text.trim().parse().unwrap_or(0),
                "path" => {
                    if let Some(id) = id {
                        paths.insert(id, unescape(text));
                    }
                }
                _ => {}
            }
        }
        let Some(gt) = rest[lt..].find('>') else { break };
        let raw = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];
        let close = raw.starts_with('/');
        let empty = raw.ends_with('/');
        let body = raw.trim_start_matches('/').trim_end_matches('/');
        // The attributes keep their leading space, which `attr` matches on.
        let (name, attrs) = body.find(' ').map(|i| (&body[..i], &body[i..])).unwrap_or((body, ""));
        let id = attr(attrs, "id").and_then(|i| i.parse::<u64>().ok());
        let rf = attr(attrs, "ref").and_then(|i| i.parse::<u64>().ok());
        if close {
            match name {
                "row" => {
                    if let Some(&(p, t)) = thread.and_then(|t| threads.get(&t))
                        && !stack.is_empty()
                    {
                        // Time Profiler writes an inlined call as its own frame at the caller's
                        // address, innermost first: one address per run of them, named by the last
                        // (the function the code is compiled into), as DWARF expands inlines again.
                        let mut addrs: Vec<u64> = Vec::with_capacity(stack.len());
                        for f in &stack {
                            let Some(x) = frames.get(f) else { continue };
                            if addrs.last() != Some(&x.addr) {
                                addrs.push(x.addr);
                            }
                            outer.insert(x.addr, *f);
                        }
                        let next = deep.stacks.len() as u32;
                        let at = *index.entry(addrs.clone()).or_insert_with(|| {
                            deep.stacks.push(addrs);
                            next
                        });
                        let ns = t0 + time;
                        t1 = t1.max(ns);
                        deep.samples.push((ns, p, t, 0, at));
                    }
                    (time, thread) = (0, None);
                    stack.clear();
                }
                "thread" => {
                    if let Some((id, name)) = def_thread.take() {
                        threads.insert(id, (pid, tid));
                        deep.threads.entry(tid).or_insert(name);
                    }
                }
                "process" => {
                    if let Some(id) = def_proc.take() {
                        procs.insert(id, pid);
                    }
                }
                "frame" => {
                    if let Some((id, f)) = def_frame.take() {
                        frames.insert(id, f);
                    }
                }
                "tagged-backtrace" | "backtrace" => {
                    if let Some(id) = def_bt.take() {
                        backtraces.insert(id, stack.clone());
                    }
                }
                _ => {}
            }
            continue;
        }
        match name {
            "sample-time" | "tid" | "pid" => text_of = Some((name, id)),
            "path" => match rf {
                Some(r) => {
                    if let Some((_, f)) = def_frame.as_mut() {
                        f.file = Some(r);
                    }
                }
                None => {
                    text_of = Some(("path", id));
                    if let Some((_, f)) = def_frame.as_mut() {
                        f.file = id;
                    }
                }
            },
            "thread" => match rf {
                Some(r) => {
                    if thread.is_none() {
                        thread = Some(r);
                    }
                }
                None => {
                    // `Main Thread (0x32bef17) (RosterEase, pid: 96199)`: the name before its id.
                    let fmt = attr(attrs, "fmt").unwrap_or_default();
                    let name = fmt.split(" (0x").next().unwrap_or("").to_string();
                    if let Some(i) = id {
                        def_thread = Some((i, name));
                        thread = thread.or(Some(i));
                    }
                }
            },
            "process" => {
                if let Some(r) = rf {
                    if let Some(&p) = procs.get(&r) {
                        pid = p;
                    }
                } else if !empty {
                    def_proc = id;
                }
            }
            "tagged-backtrace" | "backtrace" => match rf {
                Some(r) => stack = backtraces.get(&r).cloned().unwrap_or_default(),
                None if !empty => def_bt = id,
                None => {}
            },
            "frame" => match rf {
                Some(r) => stack.push(r),
                None => {
                    let f = Frame { addr: attr(attrs, "addr").and_then(|a| hex(&a)).unwrap_or(0), name: attr(attrs, "name").unwrap_or_default(), ..Frame::default() };
                    if let Some(i) = id {
                        stack.push(i);
                        if empty {
                            frames.insert(i, f);
                        } else {
                            def_frame = Some((i, f));
                        }
                    }
                }
            },
            "binary" => {
                let b = rf.or_else(|| {
                    let i = id?;
                    let path = attr(attrs, "path").unwrap_or_default();
                    let name = attr(attrs, "name").unwrap_or_default();
                    binaries.insert(i, (name, path, attr(attrs, "load-addr").and_then(|a| hex(&a)).unwrap_or(0)));
                    Some(i)
                });
                if let Some((_, f)) = def_frame.as_mut() {
                    f.binary = b;
                }
            }
            "source" => {
                if let Some((_, f)) = def_frame.as_mut() {
                    f.line = attr(attrs, "line").and_then(|l| l.parse().ok()).unwrap_or(0);
                }
            }
            _ => {}
        }
    }
    deep.t1_ns = t1;
    // A simulator app attached before dyld ran gets its simulator libraries by install name
    // (`/System/Library/Frameworks/SwiftUI.framework/SwiftUI`), not by their file in the runtime
    // (`…/RuntimeRoot/System/…`): they are the runtime's, under the root the trace's other paths name.
    // The kernel, platform and pthread libraries a simulator process takes from the host stay as they are.
    if let Some(root) = binaries.values().find_map(|b| b.1.find("/RuntimeRoot/").map(|i| b.1[..i + 12].to_string())) {
        const HOST: [&str; 3] = ["libsystem_kernel.dylib", "libsystem_platform.dylib", "libsystem_pthread.dylib"];
        for (name, path, _) in binaries.values_mut() {
            if (path.starts_with("/System/") || path.starts_with("/usr/lib/")) && !HOST.contains(&name.as_str()) {
                *path = format!("{root}{path}");
            }
        }
    }
    // Modules: each binary from its load address to the highest address sampled in it, per process.
    let pids: Vec<u32> = deep.samples.iter().map(|s| s.1).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let mut extent: HashMap<u64, u64> = HashMap::new();
    for f in frames.values() {
        if let Some(b) = f.binary {
            let e = extent.entry(b).or_default();
            *e = (*e).max(f.addr);
        }
    }
    for pid in &pids {
        for (b, (_, path, base)) in &binaries {
            if let Some(&top) = extent.get(b).filter(|&&t| t >= *base) {
                deep.modules.push(Module { pid: *pid, base: *base, size: top - base + 1, path: path.clone() });
            }
        }
        // What Time Profiler already named, for frames no debug info strobe finds names.
        for f in outer.values().filter_map(|id| frames.get(id)) {
            let image = f.binary.and_then(|b| binaries.get(&b)).map(|b| b.0.clone()).unwrap_or_default();
            let file = f.file.and_then(|p| paths.get(&p)).cloned().unwrap_or_default();
            deep.located.push((*pid, f.addr, f.name.clone(), file, f.line, image));
        }
    }
    deep
}
