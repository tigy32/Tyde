//! Opt-in ownership snapshots, not a native stack profiler or causal attribution.
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

static MAIN: OnceLock<std::thread::ThreadId> = OnceLock::new();
static SCOPE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub(crate) enum Callback {
    InvokeDispatch = 1,
    PageLoad,
    WindowEvent,
    MenuEvent,
    RunEvent,
    MainDispatchProbe,
    ClipboardRequest,
    ClipboardTargets,
    ClipboardImage,
    GatedDevFixture,
}

pub(crate) struct Scope {
    previous: u64,
    main: bool,
    // Callback ownership must never move to another thread or across an await.
    _local: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Scope {
    pub(crate) fn enter(callback: Callback) -> Self {
        let main = MAIN.get() == Some(&std::thread::current().id());
        let previous = SCOPE.load(Ordering::SeqCst);
        if main {
            SCOPE.store(
                ((previous >> 8) + 1) << 8 | callback as u64,
                Ordering::SeqCst,
            );
        }
        Self {
            previous,
            main,
            _local: std::marker::PhantomData,
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        if self.main {
            let sequence = (SCOPE.load(Ordering::SeqCst) >> 8) + 1;
            SCOPE.store(sequence << 8 | self.previous & 255, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Start,
    Snapshot,
    Stop,
    #[cfg(debug_assertions)]
    GatedDevFixture,
}

struct Request {
    action: Option<Action>,
    reply: Option<oneshot::Sender<Result<Snapshot, &'static str>>>,
}

#[derive(Clone)]
pub(crate) struct Recorder(mpsc::Sender<Request>);

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum ThreadState {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    RunningOrRunnable,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Waiting,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    UninterruptibleWait,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Stopped,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Halted,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Unrecognized,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
enum Boundary {
    DispatchPending,
    Snapshot,
    #[cfg(debug_assertions)]
    GatedDevFixture,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct Observation {
    query_start_ms: u128,
    query_end_ms: u128,
    boundary: Boundary,
    callback: Option<Callback>,
    scope_changed_during_query: bool,
    thread_state: Result<ThreadState, &'static str>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Snapshot {
    running: bool,
    capacity: usize,
    overwritten: u64,
    request_dropped: u64,
    coverage: &'static str,
    observations: VecDeque<Observation>,
}

// Only the background recorder reads the OS. These handles never enter exports.
struct MainThread {
    #[cfg(target_os = "linux")]
    tid: libc::pid_t,
    #[cfg(target_os = "macos")]
    port: libc::mach_port_t,
}

impl MainThread {
    fn current() -> Self {
        Self {
            #[cfg(target_os = "linux")]
            tid: unsafe { libc::gettid() },
            #[cfg(target_os = "macos")]
            port: unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) },
        }
    }

    async fn state(&self) -> Result<ThreadState, &'static str> {
        #[cfg(target_os = "linux")]
        {
            // Never read cmdline, environ, comm, or process arguments. Discard
            // every status field except the kernel's single state code.
            let status = tokio::fs::read_to_string(format!("/proc/self/task/{}/status", self.tid))
                .await
                .map_err(|_| "thread_status_read_failed")?;
            let state = status
                .lines()
                .find_map(|line| line.strip_prefix("State:\t"))
                .and_then(|value| value.as_bytes().first())
                .ok_or("thread_state_missing")?;
            Ok(match state {
                b'R' => ThreadState::RunningOrRunnable,
                b'S' => ThreadState::Waiting,
                b'D' => ThreadState::UninterruptibleWait,
                b'T' | b't' => ThreadState::Stopped,
                b'X' | b'Z' => ThreadState::Halted,
                _ => ThreadState::Unrecognized,
            })
        }
        #[cfg(target_os = "macos")]
        {
            let mut info = std::mem::MaybeUninit::<libc::thread_basic_info>::uninit();
            let mut count = libc::THREAD_BASIC_INFO_COUNT;
            // Public Mach query of our own main thread; no suspension or task
            // port acquisition, elevated privilege, or private framework API.
            let result = unsafe {
                libc::thread_info(
                    self.port,
                    libc::THREAD_BASIC_INFO,
                    info.as_mut_ptr().cast(),
                    &mut count,
                )
            };
            if result != libc::KERN_SUCCESS || count != libc::THREAD_BASIC_INFO_COUNT {
                return Err("thread_info_failed");
            }
            let info = unsafe { info.assume_init() };
            Ok(match info.run_state {
                libc::TH_STATE_RUNNING => ThreadState::RunningOrRunnable,
                libc::TH_STATE_WAITING => ThreadState::Waiting,
                libc::TH_STATE_UNINTERRUPTIBLE => ThreadState::UninterruptibleWait,
                libc::TH_STATE_STOPPED => ThreadState::Stopped,
                libc::TH_STATE_HALTED => ThreadState::Halted,
                _ => ThreadState::Unrecognized,
            })
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            Err("thread_state_unsupported")
        }
    }

    async fn observe(&self, start: Instant, boundary: Boundary) -> Observation {
        let query_start_ms = start.elapsed().as_millis();
        let before = SCOPE.load(Ordering::SeqCst);
        let thread_state = self.state().await;
        let after = SCOPE.load(Ordering::SeqCst);
        let callback = if before != after {
            None
        } else {
            match before & 255 {
                1 => Some(Callback::InvokeDispatch),
                2 => Some(Callback::PageLoad),
                3 => Some(Callback::WindowEvent),
                4 => Some(Callback::MenuEvent),
                5 => Some(Callback::RunEvent),
                6 => Some(Callback::MainDispatchProbe),
                7 => Some(Callback::ClipboardRequest),
                8 => Some(Callback::ClipboardTargets),
                9 => Some(Callback::ClipboardImage),
                10 => Some(Callback::GatedDevFixture),
                _ => None,
            }
        };
        Observation {
            query_start_ms,
            query_end_ms: start.elapsed().as_millis(),
            boundary,
            callback,
            scope_changed_during_query: before != after,
            thread_state,
        }
    }
}

impl Recorder {
    pub(crate) fn start(app: tauri::AppHandle) -> Self {
        let _ = MAIN.set(std::thread::current().id());
        let main = MainThread::current();
        let (sender, mut receiver) = mpsc::channel::<Request>(16);
        tauri::async_runtime::spawn(async move {
            let mut snapshot = Snapshot {
                running: false,
                capacity: 64,
                overwritten: 0,
                request_dropped: 0,
                coverage: "scoped_product_callbacks_only; null_owner_is_unknown_not_framework; OS_state_is_point_sample_not_CPU_or_cause; no_native_stack",
                observations: VecDeque::new(),
            };
            let mut start = Instant::now();
            while let Some(request) = receiver.recv().await {
                let mut boundary = Boundary::Snapshot;
                match request.action {
                    Some(Action::Start) => {
                        start = Instant::now();
                        snapshot.running = true;
                        snapshot.observations.clear();
                        snapshot.overwritten = 0;
                        snapshot.request_dropped = 0;
                    }
                    None => {
                        boundary = Boundary::DispatchPending;
                    }
                    Some(Action::Snapshot | Action::Stop) => {}
                    #[cfg(debug_assertions)]
                    Some(Action::GatedDevFixture) => {
                        let result = gated_fixture(&app, &main, start, snapshot.running).await;
                        match result {
                            Ok(row) => {
                                retain(&mut snapshot, row);
                                if let Some(reply) = request.reply {
                                    let _ = reply.send(Ok(snapshot.clone()));
                                }
                            }
                            Err(error) => {
                                if let Some(reply) = request.reply {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        continue;
                    }
                }
                if snapshot.running {
                    retain(&mut snapshot, main.observe(start, boundary).await);
                }
                if matches!(request.action, Some(Action::Stop)) {
                    snapshot.running = false;
                }
                if let Some(reply) = request.reply {
                    let _ = reply.send(Ok(snapshot.clone()));
                }
            }
            drop(app);
        });
        Self(sender)
    }

    pub(crate) async fn control(&self, action: Action) -> Result<Snapshot, &'static str> {
        let (reply, result) = oneshot::channel();
        self.0
            .send(Request {
                action: Some(action),
                reply: Some(reply),
            })
            .await
            .map_err(|_| "native_recorder_closed")?;
        result.await.map_err(|_| "native_recorder_no_reply")?
    }

    pub(crate) async fn dispatch_pending(&self) {
        // The probe has only one outstanding dispatch. Await admission instead
        // of dropping observations invisibly or streaming every callback.
        if self
            .0
            .send(Request {
                action: None,
                reply: None,
            })
            .await
            .is_err()
        {
            tracing::error!("ui.diagnostic layer=native event=owner_recorder_closed");
        }
    }
}

fn retain(snapshot: &mut Snapshot, row: Observation) {
    if snapshot.observations.len() == snapshot.capacity {
        snapshot.observations.pop_front();
        snapshot.overwritten += 1;
    }
    snapshot.observations.push_back(row);
}

#[cfg(debug_assertions)]
async fn gated_fixture(
    app: &tauri::AppHandle,
    main: &MainThread,
    start: Instant,
    running: bool,
) -> Result<Observation, &'static str> {
    if !running {
        return Err("start_native_capture_first");
    }
    if devtools_protocol::DevInstanceStartupOwner::from_env()
        .map_err(|_| "dev_owner_invalid")?
        .is_none()
    {
        return Err("owned_dev_instance_required");
    }
    let (entered, entry) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (done, completed) = oneshot::channel();
    app.run_on_main_thread(move || {
        let scope = Scope::enter(Callback::GatedDevFixture);
        let _ = entered.send(());
        let result = gate.recv().map_err(|_| "dev_gate_closed");
        drop(scope);
        let _ = done.send(result);
    })
    .map_err(|_| "dev_dispatch_failed")?;
    entry.await.map_err(|_| "dev_entry_cancelled")?;
    let row = main.observe(start, Boundary::GatedDevFixture).await;
    release.send(()).map_err(|_| "dev_release_failed")?;
    completed.await.map_err(|_| "dev_completion_cancelled")??;
    Ok(row)
}
