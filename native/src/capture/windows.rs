//! Windows capture targets: lists monitors and windows for the app's picker and
//! captures a single window through Windows.Graphics.Capture.
//!
//! A monitor stays in the video pipeline's own graph (`ddagrab`). A window cannot:
//! `gfxcapture` only produces a frame when the window changes, and on FFmpeg 9
//! `av_buffersink_get_frame` does not come back while it has none (its loop keeps
//! re-requesting, and the filter waits 1 s at a time). On the video thread that
//! would freeze the stop and quality requests, and the repeats that keep viewers
//! going, for as long as the window stays still. So a reader thread of its own
//! blocks there and hands the D3D11 frames over, the same way PipeWire hands
//! frames over on Linux.
//!
//! The price: a reader that is blocked cannot be interrupted. Stopping marks it
//! and asks the window to repaint, and it exits at the next frame. If the window
//! is minimized, that waits until it comes back, holding only the capture session.
//!
//! On Windows 10 a captured window has a yellow border that cannot be turned
//! off. While the window covers its whole monitor and is in front, it looks
//! exactly like that monitor, so it is captured through the monitor
//! (`ddagrab`, no border) and every frame is checked to still be that case.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::{Duration, Instant};

use ffmpeg_sys_next as ff;
use windows_sys::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows_sys::Win32::Graphics::Gdi::{
    GetMonitorInfoW, HMONITOR, MONITOR_DEFAULTTONULL, MONITORINFO, MonitorFromWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_FRAME, RDW_INVALIDATE, RedrawWindow,
};
use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumChildWindows, EnumWindows, GWL_EXSTYLE, GetClassNameW, GetClientRect, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SW_RESTORE, ShowWindow, WS_EX_TOOLWINDOW,
};

use super::Opened;
use super::target::{self, CaptureKind, CaptureTarget, DesktopOutput, MonitorEntry, OpenError, Resize, SourceList, WindowEntry, WindowFacts};
use crate::video::Source;
use crate::video::ffi::{self, Frame, Graph, HwDevice, Pulled};
use crate::video::source::{CpuFrame, CpuSource, Mailbox, PixelFormat, Pixels};

/* ---------------- The list ---------------- */

pub fn list() -> SourceList {
    let build = windows_build();
    let supported = target::windows_supported(build) && gfxcapture_exists();
    SourceList {
        monitors: monitors(),
        windows: if supported { windows() } else { Vec::new() },
        no_windows: (!supported).then(|| "Transmitir uma janela só funciona no Windows 10 versão 2004 ou mais novo. Dá para transmitir a tela inteira.".to_owned()),
    }
}

fn gfxcapture_exists() -> bool {
    !unsafe { ffmpeg_sys_next::avfilter_get_by_name(c"gfxcapture".as_ptr()) }.is_null()
}

/// The real build number: `GetVersionEx` lies to programs without a manifest.
fn windows_build() -> u32 {
    use windows_sys::Win32::System::SystemInformation::OSVERSIONINFOW;
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OSVERSIONINFOW) -> i32;
    }
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    if unsafe { RtlGetVersion(&mut info) } == 0 { info.dwBuildNumber } else { 0 }
}

/// Monitors in the order and with the numbers `ddagrab` uses: an output index
/// means nothing without its adapter, and neither is `EnumDisplayMonitors`' order.
fn monitors() -> Vec<MonitorEntry> {
    let out: Vec<MonitorEntry> = outputs()
        .iter()
        .enumerate()
        .map(|(i, (output, desc))| {
            let r = desc.DesktopCoordinates;
            // The primary monitor is the one at the desktop's origin.
            let primary = if r.left == 0 && r.top == 0 { " (principal)" } else { "" };
            MonitorEntry { label: format!("Tela {}{primary}, {}×{}", i + 1, r.right - r.left, r.bottom - r.top), target: CaptureTarget::Monitor(*output) }
        })
        .collect();
    if out.is_empty() { fallback_monitor() } else { out }
}

/// The DXGI outputs attached to the desktop, numbered as `ddagrab` numbers them.
fn outputs() -> Vec<(DesktopOutput, windows::Win32::Graphics::Dxgi::DXGI_OUTPUT_DESC)> {
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1};
    let mut out = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return out };
    let mut adapter_idx = 0;
    while let Ok(adapter) = unsafe { factory.EnumAdapters1(adapter_idx) } {
        let mut output_idx = 0;
        while let Ok(output) = unsafe { adapter.EnumOutputs(output_idx) } {
            if let Ok(desc) = unsafe { output.GetDesc() }
                && desc.AttachedToDesktop.as_bool()
            {
                out.push((DesktopOutput { adapter: adapter_idx, output: output_idx }, desc));
            }
            output_idx += 1;
        }
        adapter_idx += 1;
    }
    out
}

/// The DXGI output that shows this monitor (`DXGI_OUTPUT_DESC` names its `HMONITOR`).
fn output_of(monitor: HMONITOR) -> Option<DesktopOutput> {
    outputs().into_iter().find(|(_, desc)| std::ptr::eq(desc.Monitor.0, monitor)).map(|(output, _)| output)
}

fn fallback_monitor() -> Vec<MonitorEntry> {
    vec![MonitorEntry { label: "Tela inteira".into(), target: CaptureTarget::Default }]
}

fn windows() -> Vec<WindowEntry> {
    unsafe extern "system" fn each(hwnd: HWND, list: LPARAM) -> i32 {
        let list = unsafe { &mut *(list as *mut Vec<WindowEntry>) };
        let facts = facts(hwnd);
        if target::listable(&facts, unsafe { IsIconic(hwnd) } != 0) {
            list.push(WindowEntry { title: facts.title, target: CaptureTarget::Window { hwnd: hwnd as usize as u64, title: String::new() } });
        }
        TRUE
    }
    let mut list: Vec<WindowEntry> = Vec::new();
    unsafe { EnumWindows(Some(each), &mut list as *mut _ as LPARAM) };
    for e in &mut list {
        if let CaptureTarget::Window { title, .. } = &mut e.target {
            title.clone_from(&e.title);
        }
    }
    list
}

fn facts(hwnd: HWND) -> WindowFacts {
    let mut cloaked = 0u32;
    let cloaked_ok = unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED as u32, (&mut cloaked as *mut u32).cast(), 4) } == 0;
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    let (width, height) = client_size(hwnd).map_or((0, 0), |(w, h)| (w as i32, h as i32));
    WindowFacts {
        title: window_title(hwnd),
        class: class_name(hwnd),
        visible: unsafe { IsWindowVisible(hwnd) } != 0,
        cloaked: cloaked_ok && cloaked != 0,
        tool_window: ex_style & WS_EX_TOOLWINDOW != 0,
        ours: window_pid(hwnd) == Some(std::process::id()),
        width,
        height,
    }
}

fn window_title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; len as usize + 1];
    let got = unsafe { GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    let got = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..got.max(0) as usize])
}

fn client_size(hwnd: HWND) -> Option<(u32, u32)> {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    if unsafe { GetClientRect(hwnd, &mut r) } == 0 {
        return None;
    }
    Some(((r.right - r.left).max(0) as u32, (r.bottom - r.top).max(0) as u32))
}

fn rect(r: RECT) -> target::Rect {
    target::Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

fn window_rect(hwnd: HWND) -> Option<target::Rect> {
    let mut r = RECT { left: 0, top: 0, right: 0, bottom: 0 };
    (unsafe { GetWindowRect(hwnd, &mut r) } != 0).then(|| rect(r))
}

fn monitor_rect(monitor: HMONITOR) -> Option<target::Rect> {
    if monitor.is_null() {
        return None;
    }
    let mut info: MONITORINFO = unsafe { std::mem::zeroed() };
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    (unsafe { GetMonitorInfoW(monitor, &mut info) } != 0).then(|| rect(info.rcMonitor))
}

/// `through_monitor` with what Windows says right now about the window and that
/// monitor. A window that is gone, or a monitor that is, does not fill anything.
fn fills(hwnd: HWND, monitor: HMONITOR, borderless: bool) -> bool {
    let (Some(window), Some(screen)) = (window_rect(hwnd), monitor_rect(monitor)) else { return false };
    target::through_monitor(borderless, window, screen, unsafe { GetForegroundWindow() } == hwnd, unsafe { IsIconic(hwnd) } != 0)
}

fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
    (pid != 0).then_some(pid)
}

fn process_exe(pid: u32) -> String {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            return String::new();
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, buf.as_mut_ptr(), &mut len);
        CloseHandle(process);
        if ok == 0 {
            return String::new();
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        path.rsplit('\\').next().unwrap_or_default().to_owned()
    }
}

/// Processes behind a window's children (where a UWP app really runs).
fn child_pids(hwnd: HWND) -> Vec<u32> {
    unsafe extern "system" fn each(child: HWND, list: LPARAM) -> i32 {
        let list = unsafe { &mut *(list as *mut Vec<u32>) };
        if let Some(pid) = window_pid(child) {
            if !list.contains(&pid) {
                list.push(pid);
            }
        }
        TRUE
    }
    let mut list = Vec::new();
    unsafe { EnumChildWindows(hwnd, Some(each), &mut list as *mut _ as LPARAM) };
    list
}

/* ---------------- Opening ---------------- */

fn gone(title: &str) -> String {
    format!("A janela \"{title}\" foi fechada. Escolha outra.")
}

pub fn still_there(target: &CaptureTarget) -> Result<(), String> {
    match target {
        CaptureTarget::Window { hwnd, title } if unsafe { IsWindow(*hwnd as usize as HWND) } == 0 => Err(gone(title)),
        _ => Ok(()),
    }
}

/// Opens the target. For a window this already captures: what fails, fails here,
/// before anyone is told the stream is on air.
pub fn open(target: &CaptureTarget) -> Result<Opened, OpenError> {
    match target {
        CaptureTarget::Default => Ok(Opened { source: Source::Desktop(DesktopOutput::default()), kind: CaptureKind::Monitor }),
        CaptureTarget::Monitor(output) => Ok(Opened { source: Source::Desktop(*output), kind: CaptureKind::Monitor }),
        CaptureTarget::Window { hwnd, title } => {
            let handle = *hwnd as usize as HWND;
            still_there(target).map_err(OpenError::Failed)?;
            if !target::windows_supported(windows_build()) || !gfxcapture_exists() {
                return Err(OpenError::Failed("Transmitir uma janela só funciona no Windows 10 versão 2004 ou mais novo.".into()));
            }
            // A minimized window sends no frames: the person asked for this one.
            if unsafe { IsIconic(handle) } != 0 {
                unsafe { ShowWindow(handle, SW_RESTORE) };
            }
            let pid = window_pid(handle);
            let exe = pid.map(process_exe).unwrap_or_default();
            let sound = target::window_sound(pid, &exe, &child_pids(handle));
            let capture = WindowCapture::start(*hwnd).map_err(|e| OpenError::Failed(format!("Não consegui capturar a janela \"{title}\": {e}")))?;
            Ok(Opened { source: Source::Cpu(Box::new(capture)), kind: CaptureKind::Window { title: Some(title.clone()), sound } })
        }
    }
}

/* ---------------- The capture ---------------- */

const CLOSED: &str = "A janela que você estava transmitindo foi fechada. A transmissão parou.";

/// How often the watcher looks at the window.
const WATCH: Duration = Duration::from_millis(100);
/// How long a reader's filter may take to open (it waits on the capture thread).
const READER_OPEN: Duration = Duration::from_secs(5);

struct Shared {
    mailbox: Mailbox,
    ended: Mutex<Option<String>>,
    stop: AtomicBool,
}

struct WindowCapture {
    shared: Arc<Shared>,
}

impl CpuSource for WindowCapture {
    fn next(&mut self, timeout: Duration) -> Option<CpuFrame> {
        self.shared.mailbox.take(timeout)
    }

    fn ended(&self) -> Option<String> {
        self.shared.ended.lock().unwrap().clone()
    }
}

impl Drop for WindowCapture {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

impl WindowCapture {
    fn start(hwnd: u64) -> Result<Self, String> {
        let borderless = allow_borderless();
        let shared = Arc::new(Shared { mailbox: Mailbox::default(), ended: Mutex::new(None), stop: AtomicBool::new(false) });
        let reader = Reader::start(hwnd, Kind::Window, shared.clone())?;
        let s = shared.clone();
        std::thread::Builder::new().name("telinha-janela".into()).spawn(move || watch(hwnd, borderless, reader, s)).map_err(|e| e.to_string())?;
        Ok(Self { shared })
    }
}

/// How long to wait for Windows to answer the request for a capture without the border.
const BORDERLESS_WAIT: Duration = Duration::from_secs(2);

/// Windows draws a yellow border around a window being captured. `gfxcapture`
/// already turns it off for its session, but Windows only honours that once the
/// app has asked for borderless capture, which nothing did. Asked once per run;
/// the answer is whether Windows allowed it. On Windows 10 the request does not
/// exist and the border stays, so a window that fills its monitor is captured
/// through the monitor instead (see `watch`).
fn allow_borderless() -> bool {
    use windows::Graphics::Capture::{GraphicsCaptureAccess, GraphicsCaptureAccessKind};
    use windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus;
    static ALLOWED: OnceLock<bool> = OnceLock::new();
    *ALLOWED.get_or_init(|| {
        // On a thread of its own, so a request that never answers cannot hold the stream.
        let (tx, rx) = mpsc::channel();
        let spawned = std::thread::Builder::new().name("telinha-borda".into()).spawn(move || {
            let _ = tx.send(GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join()));
        });
        if let Err(e) = spawned {
            tracing::warn!("could not ask for a capture without the yellow border: {e}");
            return false;
        }
        match rx.recv_timeout(BORDERLESS_WAIT) {
            Ok(Ok(AppCapabilityAccessStatus::Allowed)) => {
                tracing::info!("window capture without the yellow border");
                true
            }
            Ok(Ok(status)) => {
                tracing::warn!(status = status.0, "Windows refused a capture without the yellow border");
                false
            }
            Ok(Err(e)) => {
                tracing::warn!("no capture without the yellow border on this Windows: {e}");
                false
            }
            Err(_) => {
                tracing::warn!("Windows did not answer the request for a capture without the yellow border");
                false
            }
        }
    })
}

/// After the monitor's capture failed, how long the window's goes on before the
/// monitor is tried again (a game changing the display mode loses the monitor's).
const MONITOR_RETRY: Duration = Duration::from_secs(5);

/// Follows the window from outside the readers, which may be blocked: reopens
/// the capture at a new size once it settles, and notices when the window goes.
/// Where the border cannot be turned off, it also moves the capture to the
/// monitor while the window fills it and is in front, and back to the window
/// when either stops (the monitor's reader stops by itself on the first frame
/// that fails this, so going back is never late for the viewers).
fn watch(hwnd: u64, borderless: bool, mut reader: Reader, shared: Arc<Shared>) {
    let handle = hwnd as usize as HWND;
    let mut size = client_size(handle);
    let mut resize = Resize::default();
    let mut monitor_again: Option<Instant> = None;
    while !shared.stop.load(Ordering::Relaxed) {
        std::thread::sleep(WATCH);
        if unsafe { IsWindow(handle) } == 0 {
            shared.ended.lock().unwrap().get_or_insert_with(|| CLOSED.to_owned());
            break;
        }
        // Minimized, a window reports no size: that is not a resize. A monitor
        // reader drops its frames meanwhile (the window is not in front).
        if unsafe { IsIconic(handle) } != 0 {
            continue;
        }
        let now = Instant::now();
        if reader.left() == Some(Left::Failed) {
            monitor_again = Some(now + MONITOR_RETRY);
        }
        let monitor = unsafe { MonitorFromWindow(handle, MONITOR_DEFAULTTONULL) };
        let want = monitor_again.is_none_or(|at| now >= at) && fills(handle, monitor, borderless);
        let on_monitor = matches!(reader.kind, Kind::Monitor { .. });
        if on_monitor && want && reader.left().is_none() {
            continue;
        }
        if want {
            match monitor_reader(hwnd, monitor, borderless, &shared) {
                Ok(next) => {
                    if !on_monitor {
                        tracing::info!("window fills its monitor: capturing the monitor");
                    }
                    reader = next;
                    continue;
                }
                Err(e) => {
                    tracing::warn!("the window fills its monitor, but the monitor cannot be captured, so the window still is: {e}");
                    monitor_again = Some(now + MONITOR_RETRY);
                }
            }
        }
        if on_monitor {
            // The window no longer fills its monitor or is not in front, or the
            // monitor's capture failed: the window again, at its current size.
            match Reader::start(hwnd, Kind::Window, shared.clone()) {
                Ok(next) => {
                    tracing::info!("capturing the window again");
                    reader = next;
                    size = client_size(handle);
                    resize.clear();
                }
                Err(e) => tracing::warn!("going back to the window capture: {e}"),
            }
            continue;
        }
        let now_size = client_size(handle).filter(|(w, h)| *w > 0 && *h > 0);
        match now_size {
            Some(s) if Some(s) != size => {
                resize.saw(s, now);
                if resize.due(now) {
                    resize.clear();
                    match Reader::start(hwnd, Kind::Window, shared.clone()) {
                        Ok(next) => {
                            reader = next;
                            size = Some(s);
                        }
                        Err(e) => tracing::warn!("reopening the window capture at the new size: {e}"),
                    }
                }
            }
            Some(_) => resize.clear(),
            None => {}
        }
    }
    drop(reader);
}

/// A reader of the monitor the window is on, if DXGI knows that monitor.
fn monitor_reader(hwnd: u64, monitor: HMONITOR, borderless: bool, shared: &Arc<Shared>) -> Result<Reader, String> {
    let output = output_of(monitor).ok_or("its monitor is not among the DXGI outputs")?;
    Reader::start(hwnd, Kind::Monitor { output, monitor: monitor as usize, borderless }, shared.clone())
}

/// A repaint, so a blocked reader sees a frame and can exit.
fn poke(hwnd: HWND) {
    if unsafe { IsWindow(hwnd) } != 0 {
        unsafe { RedrawWindow(hwnd, std::ptr::null(), std::ptr::null_mut(), RDW_INVALIDATE | RDW_ERASE | RDW_FRAME | RDW_ALLCHILDREN) };
    }
}

/// What a reader captures.
#[derive(Debug, Clone, Copy)]
enum Kind {
    /// The window itself, through `gfxcapture`.
    Window,
    /// The monitor the window fills, through `ddagrab`, for as long as it does.
    Monitor {
        output: DesktopOutput,
        /// HMONITOR, as a number so it can cross threads.
        monitor: usize,
        borderless: bool,
    },
}

/// Why a monitor's reader stopped by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Left {
    /// A frame came while the window did not fill the monitor or was not in front.
    NotFilled,
    /// The capture failed or ended.
    Failed,
}

/// One open capture (`gfxcapture` at one size, or `ddagrab`), on a thread of its
/// own. Dropping it stops whichever it is.
struct Reader {
    stop: Arc<AtomicBool>,
    hwnd: u64,
    kind: Kind,
    /// Set when a monitor's reader stopped by itself.
    left: Arc<OnceLock<Left>>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // `ddagrab` returns frames on its own; only `gfxcapture` waits for the window to change.
        if matches!(self.kind, Kind::Window) {
            poke(self.hwnd as usize as HWND);
        }
    }
}

/// `ddagrab` only reaches the outputs of the adapter its device belongs to, and
/// by default it picks the first (as `Origin::desktop_device` in the pipeline).
fn open_monitor(output: DesktopOutput) -> ffi::Result<(Graph, Option<HwDevice>)> {
    let device = if output.adapter == 0 { None } else { Some(HwDevice::create(ff::AVHWDeviceType::AV_HWDEVICE_TYPE_D3D11VA, Some(&output.adapter.to_string()))?) };
    let graph = Graph::new(&target::ddagrab_filter(output, target::MAX_FPS), None, device.as_ref())?;
    Ok((graph, device))
}

impl Reader {
    fn start(hwnd: u64, kind: Kind, shared: Arc<Shared>) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let mine = stop.clone();
        let left = Arc::new(OnceLock::new());
        let gone = left.clone();
        let (opened, result) = mpsc::channel();
        let name = match kind {
            Kind::Window => "telinha-wgc",
            Kind::Monitor { .. } => "telinha-tela",
        };
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let open = match kind {
                    Kind::Window => Graph::new(&target::gfxcapture_filter(hwnd), None, None).map(|g| (g, None)),
                    Kind::Monitor { output, .. } => open_monitor(output),
                };
                let (mut graph, _device) = match open {
                    Ok(g) => {
                        let _ = opened.send(Ok(()));
                        g
                    }
                    Err(e) => {
                        let _ = opened.send(Err(e.0));
                        return;
                    }
                };
                let mut frame = Frame::new();
                loop {
                    let pulled = graph.pull_source(&mut frame);
                    if mine.load(Ordering::Relaxed) || shared.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    match (pulled, kind) {
                        (Ok(Pulled::Frame), _) => {
                            // A monitor shows whatever is on it: its frame goes on only if the
                            // window fills it and is in front now. The first that fails ends
                            // this reader, so no later copy of an older screen goes on either.
                            if let Kind::Monitor { monitor, borderless, .. } = kind
                                && !fills(hwnd as usize as HWND, monitor as HMONITOR, borderless)
                            {
                                let _ = gone.set(Left::NotFilled);
                                return;
                            }
                            let (width, height) = frame.size();
                            let data = Pixels::D3d11(std::mem::replace(&mut frame, Frame::new()));
                            shared.mailbox.put(CpuFrame { data, width, height, stride: 0, pixel: PixelFormat::Bgrx, captured: Instant::now() });
                        }
                        (Ok(Pulled::Nothing), _) => std::thread::sleep(Duration::from_millis(5)),
                        // The window's capture goes back to the window instead of ending.
                        (Ok(Pulled::End), Kind::Monitor { .. }) => {
                            tracing::warn!("the monitor capture ended");
                            let _ = gone.set(Left::Failed);
                            return;
                        }
                        (Err(e), Kind::Monitor { .. }) => {
                            tracing::warn!("monitor capture: {e}");
                            let _ = gone.set(Left::Failed);
                            return;
                        }
                        (Ok(Pulled::End), Kind::Window) => {
                            shared.ended.lock().unwrap().get_or_insert_with(|| CLOSED.to_owned());
                            return;
                        }
                        (Err(e), Kind::Window) => {
                            tracing::warn!("window capture: {e}");
                            shared.ended.lock().unwrap().get_or_insert_with(|| "A captura da janela falhou. A transmissão parou.".to_owned());
                            return;
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        match result.recv_timeout(READER_OPEN) {
            Ok(Ok(())) => Ok(Self { stop, hwnd, kind, left }),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                stop.store(true, Ordering::Relaxed);
                Err("o sistema demorou demais para abrir a captura".into())
            }
        }
    }

    /// Why a monitor's reader stopped by itself, if it did.
    fn left(&self) -> Option<Left> {
        self.left.get().copied()
    }
}
