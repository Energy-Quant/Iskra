//! Windows monitor/fullscreen probe producing the same `HyprEvent` stream
//! as the hyprctl poller on Wayland. Each monitor is its own "workspace"
//! (id = index) so a fullscreen window only hides the pet on its output.
//! A monitor counts as fullscreen when the topmost visible window in
//! z-order intersecting it covers its full `rcMonitor` and is not maximized
//! (`IsZoomed`). The z-order test is what keeps transparent always-visible
//! shell layers (TextInputHost's screen-spanning input window, shell
//! surfaces) from permanently hiding the pet: they sit below every real
//! window, so they only decide the outcome when the desktop is genuinely
//! empty — and then they do not cover the monitor anyway. A maximized
//! window is never fullscreen, matching the Wayland eFullscreenState
//! semantics (1 = maximized, >=2 = fullscreen); the failure direction is
//! fail-open (pet stays visible) rather than fail-closed.
//!
//! Coordinates are physical pixels; the process is made DPI-aware by
//! `desktop_win::run` before the poller starts.

use crate::hyprland::{ClientInfo, HyprEvent, MonitorInfo};
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use windows_sys::core::BOOL;
use windows_sys::Win32::Foundation::{HWND, LPARAM, RECT, TRUE};
use windows_sys::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR, MONITORINFOEXW,
    MONITOR_DEFAULTTONEAREST,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetTopWindow, GetWindow, GetWindowRect,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, GW_HWNDNEXT,
};

const POLL_INTERVAL: std::time::Duration = crate::hyprland::POLL_INTERVAL;

/// Shell surfaces that span the full screen but are never an app's
/// fullscreen window — without this denylist every poll reports hidden.
const NOT_FULLSCREEN_CLASSES: [&str; 6] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "DV2ControlHost",
    "WindowsShellExperienceHost",
];

struct RawMonitor {
    rect: RECT,
    work: RECT,
    name: String,
    primary: bool,
    hmon: HMONITOR,
}

unsafe extern "system" fn enum_monitors_cb(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let out = &mut *(lparam as *mut Vec<HMONITOR>);
    out.push(hmon);
    TRUE
}

fn collect_monitors() -> Result<Vec<RawMonitor>, String> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    if unsafe {
        EnumDisplayMonitors(
            std::ptr::null_mut(),
            std::ptr::null(),
            Some(enum_monitors_cb),
            &mut handles as *mut _ as LPARAM,
        )
    } == 0
    {
        return Err("EnumDisplayMonitors failed".to_string());
    }
    let mut out = Vec::with_capacity(handles.len());
    for hmon in handles {
        let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(hmon, &mut info as *mut _ as *mut _) } == 0 {
            continue;
        }
        let end = info
            .szDevice
            .iter()
            .position(|c| *c == 0)
            .unwrap_or(info.szDevice.len());
        // `\\.\DISPLAY1` -> `DISPLAY1` so `--monitor DISPLAY2` reads like
        // the Wayland connector name instead of a UNC-ish device path.
        let name = String::from_utf16_lossy(&info.szDevice[..end]);
        out.push(RawMonitor {
            rect: info.monitorInfo.rcMonitor,
            work: info.monitorInfo.rcWork,
            name: name.strip_prefix(r"\\.\").unwrap_or(&name).to_string(),
            primary: info.monitorInfo.dwFlags & 1 != 0, // MONITORINFOF_PRIMARY
            hmon,
        });
    }
    if out.is_empty() {
        return Err("no monitors reported".to_string());
    }
    Ok(out)
}

fn rect_covers(inner: &RECT, outer: &RECT) -> bool {
    inner.left <= outer.left
        && inner.top <= outer.top
        && inner.right >= outer.right
        && inner.bottom >= outer.bottom
}

fn window_class(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = unsafe { GetClassNameW(hwnd, buf.as_mut_ptr(), buf.len() as i32) };
    if n <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..n as usize])
}

/// A visible top-level window that survived the shell/own-pid denylist,
/// in z-order (top first) as returned by the `GetTopWindow`/`GW_HWNDNEXT`
/// walk — the only z-order enumeration with documented ordering.
struct ZWindow {
    rect: RECT,
    zoomed: bool,
}

fn rects_intersect(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && a.right > b.left && a.top < b.bottom && a.bottom > b.top
}

/// Per monitor: the first z-order window intersecting it decides — the
/// monitor is fullscreen exactly when that window covers it and is not
/// maximized. Shell input layers below every real window never decide
/// unless the desktop is empty, where they do not decide for fullscreen.
fn decide_fullscreen(monitors: &[RECT], zordered: &[ZWindow]) -> Vec<bool> {
    let mut out = vec![false; monitors.len()];
    let mut decided = vec![false; monitors.len()];
    for w in zordered {
        for (i, mon) in monitors.iter().enumerate() {
            if decided[i] || !rects_intersect(&w.rect, mon) {
                continue;
            }
            decided[i] = true;
            out[i] = !w.zoomed && rect_covers(&w.rect, mon);
        }
        if decided.iter().all(|d| *d) {
            break;
        }
    }
    out
}

/// Visible non-iconic top-level windows in documented z-order, minus our
/// own pet/HUD windows and the shell surfaces in the denylist.
fn collect_windows_zorder() -> Vec<ZWindow> {
    let own = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() };
    let mut out = Vec::new();
    let mut hwnd = unsafe { GetTopWindow(std::ptr::null_mut()) };
    let mut guard = 0;
    while !hwnd.is_null() && guard < 1024 {
        guard += 1;
        let visible = unsafe { IsWindowVisible(hwnd) } != 0;
        let iconic = unsafe { IsIconic(hwnd) } != 0;
        if visible && !iconic {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, &mut pid) };
            if pid != own {
                let cls = window_class(hwnd);
                if !NOT_FULLSCREEN_CLASSES.iter().any(|c| *c == cls) {
                    let mut rc: RECT = unsafe { std::mem::zeroed() };
                    if unsafe { GetWindowRect(hwnd, &mut rc) } != 0 {
                        out.push(ZWindow {
                            rect: rc,
                            zoomed: unsafe { IsZoomed(hwnd) } != 0,
                        });
                    }
                }
            }
        }
        hwnd = unsafe { GetWindow(hwnd, GW_HWNDNEXT) };
    }
    out
}

/// One probe: monitor list plus fullscreen occupancy per monitor index.
fn probe() -> Result<(Vec<MonitorInfo>, Vec<ClientInfo>), String> {
    let raws = collect_monitors()?;
    let zordered = collect_windows_zorder();
    let fullscreen_on =
        decide_fullscreen(&raws.iter().map(|m| m.rect).collect::<Vec<_>>(), &zordered);
    let fg = unsafe { GetForegroundWindow() };
    let fg_mon = if fg.is_null() {
        std::ptr::null_mut()
    } else {
        unsafe { MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST) }
    };
    let monitors: Vec<MonitorInfo> = raws
        .iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            id: i as i64,
            name: m.name.clone(),
            logical_w: m.rect.right - m.rect.left,
            logical_h: m.rect.bottom - m.rect.top,
            reserved: [
                m.work.left - m.rect.left,
                m.work.top - m.rect.top,
                m.rect.right - m.work.right,
                m.rect.bottom - m.work.bottom,
            ],
            work_x: m.work.left,
            work_y: m.work.top,
            focused: if fg_mon.is_null() {
                m.primary
            } else {
                m.hmon == fg_mon
            },
            // Each output is its own workspace so fullscreen only hides
            // the pet on the covered monitor.
            active_workspace: i as i64,
            special_workspace: None,
        })
        .collect();
    let clients: Vec<ClientInfo> = fullscreen_on
        .iter()
        .enumerate()
        .filter(|(_, on)| **on)
        .map(|(i, _)| ClientInfo {
            mapped: true,
            fullscreen: true,
            workspace: i as i64,
        })
        .collect();
    Ok((monitors, clients))
}

/// Poll at POLL_INTERVAL until `stop` is set; emits the same events the
/// Wayland hyprctl poller does, so `desktop_shared::tick` is unchanged.
pub fn spawn_poller(tx: mpsc::Sender<HyprEvent>, stop: Arc<AtomicBool>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("winmon-poll".to_string())
        .spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match probe() {
                    Ok((monitors, clients)) => {
                        let _ = tx.send(HyprEvent::State { monitors, clients });
                    }
                    Err(e) => {
                        let _ = tx.send(HyprEvent::Unavailable(e));
                    }
                }
                for _ in 0..10 {
                    if stop.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(POLL_INTERVAL / 10);
                }
            }
        })
        .expect("spawn winmon poller")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rc(l: i32, t: i32, r: i32, b: i32) -> RECT {
        RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    fn zw(rect: RECT, zoomed: bool) -> ZWindow {
        ZWindow { rect, zoomed }
    }

    fn two_monitors() -> Vec<RECT> {
        vec![rc(0, 0, 1600, 900), rc(1600, 0, 3520, 1080)]
    }

    #[test]
    fn shell_input_layer_below_real_window_is_not_fullscreen() {
        // TextInputHost covers monitor 1 exactly but sits below the IDE;
        // the IDE decides the monitor and does not cover it.
        let z = vec![
            zw(rc(100, 100, 900, 700), false),
            zw(rc(0, 0, 1600, 900), false),
        ];
        assert_eq!(decide_fullscreen(&two_monitors(), &z), vec![false, false]);
    }

    #[test]
    fn covering_window_on_top_is_fullscreen() {
        let z = vec![zw(rc(0, 0, 1600, 900), false)];
        assert_eq!(decide_fullscreen(&two_monitors(), &z), vec![true, false]);
    }

    #[test]
    fn maximized_covering_window_is_not_fullscreen() {
        // Auto-hidden taskbar: a maximized window can cover rcMonitor fully.
        let z = vec![zw(rc(0, 0, 1600, 900), true)];
        assert_eq!(decide_fullscreen(&two_monitors(), &z), vec![false, false]);
    }

    #[test]
    fn fullscreen_decision_is_per_monitor() {
        // Borderless video covering monitor 2 while a small window floats
        // on monitor 1: only monitor 2 is fullscreen.
        let z = vec![
            zw(rc(200, 200, 700, 500), false),
            zw(rc(1600, 0, 3520, 1080), false),
        ];
        assert_eq!(decide_fullscreen(&two_monitors(), &z), vec![false, true]);
    }

    #[test]
    fn empty_desktop_is_not_fullscreen() {
        assert_eq!(decide_fullscreen(&two_monitors(), &[]), vec![false, false]);
    }
}
