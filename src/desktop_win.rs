//! Win32 layered-window backend: the Windows port of the GTK layer-shell
//! desktop. Two topmost layered windows (sprite + HUD) updated through
//! `UpdateLayeredWindow` with premultiplied BGRA frames; a SetTimer tick
//! drives the shared `desktop_shared` state machine; a named pipe plus a
//! session mutex replace the GTK single-instance command-line forward.
//!
//! Parity notes vs the Wayland build:
//! - Margin-local coordinates map to absolute screen pixels via the
//!   monitor's `work_x`/`work_y` (rcWork origin) from `winmon`.
//! - `--monitor` takes a Windows device name like `DISPLAY2`.
//! - Chat jump still invokes `zeron zeron://open/chat/<id>`; there is no
//!   compositor dispatch, so window focus is left to Zeron itself.
//! - The badge is an opaque angular box (2px frame, pink left accent by
//!   status kind, scanline rows) mirroring install_css' palette.

use crate::cli::{self, Cli};
use crate::desktop_shared::*;
use crate::{assets, behavior, hyprland, launcher, model, motion as mot, usage, winmon, zeron};
use std::cell::RefCell;
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, COLORREF, GENERIC_READ, GENERIC_WRITE, HWND, INVALID_HANDLE_VALUE,
    LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetDC, ReleaseDC, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS,
    DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, DT_WORDBREAK, FF_MODERN, FIXED_PITCH, FW_BOLD,
    FW_NORMAL, HBITMAP, HDC, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS, TRANSPARENT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_MESSAGE,
    PIPE_TYPE_MESSAGE, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{
    CreateMutexW, OpenProcess, TerminateProcess, PROCESS_TERMINATE,
};
use windows_sys::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetCapture, ReleaseCapture, SetCapture};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const ERROR_ALREADY_EXISTS: u32 = 183;
const ERROR_PIPE_CONNECTED: u32 = 535;
const ERROR_PIPE_BUSY: u32 = 231;
const ERROR_FILE_NOT_FOUND: u32 = 2;

/// Session-scoped single instance: one mutex gate + one reply pipe.
const MUTEX_NAME: &str = r"Local\zeron-pet-single-v1";
const PIPE_NAME: &str = r"\\.\pipe\zeron-pet-ctl-v1";

const WM_CTRL: u32 = WM_APP + 1;
const IDT_TICK: usize = 1;

const IDM_PAUSE: usize = 1;
const IDM_CLICKTHROUGH: usize = 2;
const IDM_QUIT: usize = 3;
const IDM_ALL_CHATS: usize = 4;
const IDM_CHAT_BASE: usize = 100;
const IDM_JUMP_BASE: usize = 400;
const MAX_MENU_CHATS: usize = 24;

// Badge palette — the outrun retrowave set from desktop.rs install_css.
// COLORREF is 0x00BBGGRR.
const C_VOID: COLORREF = 0x00090106; // #060109
const C_ROAD: COLORREF = 0x00100516; // #160510
const C_GRID: COLORREF = 0x002E124A; // #4a122e
const C_INK: COLORREF = 0x0008020B; // #0b0208
const C_AMBER: COLORREF = 0x0042AEFF; // #ffae42
const C_ORANGE: COLORREF = 0x001A7AFF; // #ff7a1a
const C_RED: COLORREF = 0x004E3BFF; // #ff3b4e
const C_PINK: COLORREF = 0x006D2AFF; // #ff2a6d
const C_CREAM: COLORREF = 0x00C9E9FF; // #ffe9c9
const C_DIM: COLORREF = 0x00639AD9; // #d99a63
const C_GHOST: COLORREF = 0x005C4B8A; // #8a4b5c

const BW: i32 = 2; // badge border
const BPX: i32 = 8; // badge padding x
const BPY: i32 = 3; // badge padding y
const SHADOW: i32 = 2; // crisp offset shadow, alpha ~0.85
const NAME_W: i32 = 34; // provider name column
const CELL_GAP: i32 = 4;
const BAR_H: i32 = 4;

fn kind_color(kind: model::StatusKind) -> COLORREF {
    match kind {
        model::StatusKind::Working => C_AMBER,
        model::StatusKind::AwaitingInput => C_ORANGE,
        model::StatusKind::Success => C_CREAM,
        model::StatusKind::Error => C_RED,
        model::StatusKind::Idle | model::StatusKind::Unknown => C_GHOST,
        model::StatusKind::Disconnected => C_DIM,
    }
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Monitor-local margin position -> absolute virtual-screen pixels.
fn screen_pos(s: &Shared, p: (i32, i32)) -> (i32, i32) {
    s.monitor_name
        .as_ref()
        .and_then(|n| s.monitors.iter().find(|m| &m.name == n))
        .map(|m| (m.work_x + p.0, m.work_y + p.1))
        .unwrap_or(p)
}

fn cursor_pos() -> POINT {
    let mut p = POINT { x: 0, y: 0 };
    unsafe { GetCursorPos(&mut p) };
    p
}

// ---- DIB surface -----------------------------------------------------------

/// A top-down 32bpp DIB section selected into a memory DC: the blit
/// source for UpdateLayeredWindow and the GDI drawing target for the HUD.
struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    fn new(w: i32, h: i32) -> Option<Dib> {
        if w <= 0 || h <= 0 {
            return None;
        }
        unsafe {
            let screen = GetDC(std::ptr::null_mut());
            let dc = CreateCompatibleDC(screen);
            ReleaseDC(std::ptr::null_mut(), screen);
            if dc.is_null() {
                return None;
            }
            let mut bmi: BITMAPINFO = std::mem::zeroed();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down: row 0 is the top pixel row
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                biSizeImage: 0,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            let mut bits: *mut c_void = std::ptr::null_mut();
            let bmp =
                CreateDIBSection(dc, &bmi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if bmp.is_null() || bits.is_null() {
                DeleteDC(dc);
                return None;
            }
            let old = SelectObject(dc, bmp);
            Some(Dib {
                dc,
                bmp,
                old,
                bits: bits as *mut u8,
                w,
                h,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.bits, (self.w * self.h * 4) as usize) }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bmp);
            DeleteDC(self.dc);
        }
    }
}

fn ensure_dib(slot: &mut Option<Dib>, w: i32, h: i32) {
    if slot.as_ref().is_some_and(|d| d.w == w && d.h == h) {
        return;
    }
    *slot = Dib::new(w, h);
}

/// Write one BGRA pixel; `color` is a COLORREF (0x00BBGGRR). For a < 255
/// alpha the color channels are premultiplied in place.
fn set_px(buf: &mut [u8], stride: i32, x: i32, y: i32, color: COLORREF, a: u8) {
    let i = ((y * stride + x) * 4) as usize;
    if i + 4 > buf.len() {
        return;
    }
    let (b, g, r) = ((color >> 16) & 0xff, (color >> 8) & 0xff, color & 0xff);
    if a == 255 {
        buf[i..i + 4].copy_from_slice(&[b as u8, g as u8, r as u8, a]);
    } else {
        let pm = |c: u32| ((c * a as u32 + 127) / 255) as u8;
        buf[i..i + 4].copy_from_slice(&[pm(b), pm(g), pm(r), a]);
    }
}

fn fill_rect_px(buf: &mut [u8], stride: i32, w: i32, h: i32, r: RECT, color: COLORREF, a: u8) {
    let (x0, y0) = (r.left.max(0), r.top.max(0));
    let (x1, y1) = (r.right.min(w), r.bottom.min(h));
    for y in y0..y1 {
        for x in x0..x1 {
            set_px(buf, stride, x, y, color, a);
        }
    }
}

fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

/// Push `dib` into `hwnd`'s layered content at absolute screen `pos`.
fn ulw(hwnd: HWND, pos: (i32, i32), dib: &Dib) {
    let dst = POINT { x: pos.0, y: pos.1 };
    let sz = SIZE {
        cx: dib.w,
        cy: dib.h,
    };
    let src = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    unsafe {
        UpdateLayeredWindow(
            hwnd,
            std::ptr::null_mut(),
            &dst,
            &sz,
            dib.dc,
            &src,
            0,
            &blend,
            ULW_ALPHA,
        );
    }
}

// ---- Application state ------------------------------------------------------

struct Fonts {
    primary: HFONT,
    secondary: HFONT,
    small: HFONT,
}

struct DragCtx {
    /// Motion coords at press time.
    mx: f64,
    my: f64,
    /// Screen cursor position at press time.
    cx: i32,
    cy: i32,
}

struct AppState {
    sh: Rc<RefCell<Shared>>,
    sprite: HWND,
    hud: HWND,
    sprite_dib: Option<Dib>,
    hud_dib: Option<Dib>,
    /// Full HUD surface size (content + pad + shadow room).
    hud_size: (i32, i32),
    badge_rect: RECT,
    hud_dirty: bool,
    hud_key: String,
    usage_snap: usage::UsageSnapshot,
    shown: bool,
    hud_shown: bool,
    ct_applied: bool,
    /// (anim, frame index, x, y) of the last blit — unchanged frames are
    /// not re-copied and re-presented every tick.
    blit_key: Option<(String, usize, i32, i32)>,
    rx_z: mpsc::Receiver<zeron::EngineMsg>,
    rx_h: mpsc::Receiver<hyprland::HyprEvent>,
    rx_u: mpsc::Receiver<usage::UsageSnapshot>,
    child_pid: Arc<Mutex<Option<u32>>>,
    drag: Option<DragCtx>,
    fonts: Fonts,
}

/// Serialized control reply; read back by `control_client`.
#[derive(serde::Serialize, serde::Deserialize)]
struct CtrlReply {
    code: i32,
    out: String,
    err: String,
}

struct CtrlRequest {
    args: Vec<String>,
    reply: mpsc::Sender<CtrlReply>,
}

// ---- Sprite surface ---------------------------------------------------------

fn blit_sprite(st: &mut AppState, frame: &assets::Frame, pos: (i32, i32)) {
    ensure_dib(&mut st.sprite_dib, frame.w, frame.h);
    let Some(dib) = st.sprite_dib.as_mut() else {
        return;
    };
    dib.pixels().copy_from_slice(&frame.bgra);
    let dib = st.sprite_dib.as_ref().unwrap();
    ulw(st.sprite, pos, dib);
}

// ---- HUD surface ------------------------------------------------------------

fn draw_text(dc: HDC, font: HFONT, color: COLORREF, r: &mut RECT, text: &str, flags: u32) -> i32 {
    unsafe {
        let prev = SelectObject(dc, font);
        SetTextColor(dc, color);
        SetBkMode(dc, TRANSPARENT as i32);
        let mut t = w(text);
        let n = DrawTextW(dc, t.as_mut_ptr(), -1, r, flags | DT_NOPREFIX);
        SelectObject(dc, prev);
        n
    }
}

/// Bar fill color per provider/state; a gradient is approximated by its
/// amber/pink midpoint — the bar is 4px high.
fn bar_color(provider: usage::Provider, state: usage::WindowState) -> COLORREF {
    let base = match provider {
        usage::Provider::Devin => C_AMBER,
        usage::Provider::Codex => C_PINK,
    };
    match state {
        usage::WindowState::Live => base,
        usage::WindowState::Stale => mix(base, C_GRID, 0.5),
        usage::WindowState::Unavailable | usage::WindowState::ResetPending => {
            mix(C_GHOST, C_GRID, 0.4)
        }
    }
}

/// Linear COLORREF blend toward `bg` by `t` (0..1).
fn mix(fg: COLORREF, bg: COLORREF, t: f64) -> COLORREF {
    let ch = |shift| {
        let f = ((fg >> shift) & 0xffu32) as f64;
        let b = ((bg >> shift) & 0xffu32) as f64;
        (f + (b - f) * t).round() as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

fn cell_text_color(state: usage::WindowState) -> COLORREF {
    match state {
        usage::WindowState::Live => C_CREAM,
        usage::WindowState::Stale => C_DIM,
        usage::WindowState::Unavailable | usage::WindowState::ResetPending => C_GHOST,
    }
}

/// Re-render the HUD DIB for the current display/usage state and push it
/// through UpdateLayeredWindow at `pos`. Layout mirrors the CSS badge:
/// angular 2px frame (left accent tinted by status kind), scanline body,
/// primary status word, secondary detail line, two provider gauge rows.
fn render_hud(st: &mut AppState, display: &model::DisplayState) {
    let (primary, secondary, kind, compact) = {
        let s = st.sh.borrow();
        (
            display.kind.label().to_string(),
            s.badge_secondary(display),
            display.kind,
            usage_compact(HUD_WIDTH),
        )
    };

    // Content width inside the badge frame.
    let content_w = HUD_WIDTH - 2 * BPX;

    // Measure against a scratch DC before allocating the real DIB.
    let measure_dc = unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        ReleaseDC(std::ptr::null_mut(), screen);
        dc
    };
    if measure_dc.is_null() {
        return;
    }
    let mut pr = rect(0, 0, content_w, 0);
    let primary_h = draw_text(
        measure_dc,
        st.fonts.primary,
        C_CREAM,
        &mut pr,
        &primary,
        DT_CENTER | DT_WORDBREAK | DT_CALCRECT,
    )
    .max(1);
    let secondary_h = if secondary.is_empty() { 0 } else { 13 };
    let row_h = 16;
    let badge_h = BPY
        + primary_h
        + if secondary_h > 0 { 2 + secondary_h } else { 0 }
        + 4
        + 1
        + 3
        + 2 * row_h
        + CELL_GAP
        + BPY;
    let hud_w = HUD_WIDTH + 2 * HUD_SHADOW_PAD;
    let hud_h = badge_h + 2 * HUD_SHADOW_PAD + SHADOW;

    ensure_dib(&mut st.hud_dib, hud_w, hud_h);
    let Some(dib) = st.hud_dib.as_mut() else {
        unsafe { DeleteDC(measure_dc) };
        return;
    };
    // `buf` below mutably borrows the DIB — capture its scalars first.
    let (dc, w_px, h_px) = (dib.dc, dib.w, dib.h);
    let buf = dib.pixels();
    buf.fill(0);

    // Badge box: (PAD,PAD)-(w-PAD, h-PAD-SHADOW); shadow offset +2,+2.
    let badge = rect(
        HUD_SHADOW_PAD,
        HUD_SHADOW_PAD,
        w_px - HUD_SHADOW_PAD,
        h_px - HUD_SHADOW_PAD - SHADOW,
    );
    let shadow = rect(
        badge.left + SHADOW,
        badge.top + SHADOW,
        badge.right + SHADOW,
        badge.bottom + SHADOW,
    );
    for y in 0..h_px {
        for x in 0..w_px {
            let in_badge = x >= badge.left && x < badge.right && y >= badge.top && y < badge.bottom;
            if in_badge {
                continue;
            }
            if x >= shadow.left && x < shadow.right && y >= shadow.top && y < shadow.bottom {
                set_px(buf, w_px, x, y, C_INK, 216);
            }
        }
    }
    // Scanline body: alternating 2px void/road rows over an opaque fill.
    for y in badge.top..badge.bottom {
        let c = if (y - badge.top) % 4 < 2 {
            C_VOID
        } else {
            C_ROAD
        };
        for x in badge.left..badge.right {
            set_px(buf, w_px, x, y, c, 255);
        }
    }
    // Angular frame: 2px amber, 2px kind-tinted left accent.
    let accent = kind_color(kind);
    fill_rect_px(
        buf,
        w_px,
        w_px,
        h_px,
        rect(badge.left, badge.top, badge.right, badge.top + BW),
        C_AMBER,
        255,
    );
    fill_rect_px(
        buf,
        w_px,
        w_px,
        h_px,
        rect(badge.left, badge.bottom - BW, badge.right, badge.bottom),
        C_AMBER,
        255,
    );
    fill_rect_px(
        buf,
        w_px,
        w_px,
        h_px,
        rect(badge.right - BW, badge.top, badge.right, badge.bottom),
        C_AMBER,
        255,
    );
    fill_rect_px(
        buf,
        w_px,
        w_px,
        h_px,
        rect(badge.left, badge.top, badge.left + BW, badge.bottom),
        accent,
        255,
    );

    // Text: GDI blends RGB against the scanline pixels already laid
    // down; alpha stays 255 everywhere inside the badge.
    let inner_l = badge.left + BW + BPX - 2;
    let inner_r = badge.right - BW - BPX + 2;
    let mut cy = badge.top + BW + BPY;
    {
        let mut r = rect(inner_l, cy, inner_r, cy + primary_h);
        draw_text(
            dc,
            st.fonts.primary,
            C_CREAM,
            &mut r,
            &primary,
            DT_CENTER | DT_WORDBREAK,
        );
        cy += primary_h;
    }
    if secondary_h > 0 {
        cy += 2;
        let mut r = rect(inner_l, cy, inner_r, cy + secondary_h);
        draw_text(
            dc,
            st.fonts.secondary,
            C_DIM,
            &mut r,
            &secondary,
            DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS,
        );
        cy += secondary_h;
    }
    // Separator.
    cy += 4;
    fill_rect_px(
        buf,
        w_px,
        w_px,
        h_px,
        rect(inner_l, cy, inner_r, cy + 1),
        C_GRID,
        255,
    );
    cy += 4;

    // Usage rows: name column + two cells (percent text over bar track).
    let cell_w = usage_cell_width(HUD_WIDTH);
    let cells_w = inner_r - inner_l - NAME_W - CELL_GAP;
    let each_w = (cells_w - CELL_GAP) / 2;
    let brush = unsafe { CreateSolidBrush(C_GRID) };
    for (ri, p) in st.usage_snap.providers.iter().enumerate() {
        let y = cy + ri as i32 * (row_h + CELL_GAP);
        let name = if compact {
            match p.provider {
                usage::Provider::Devin => "D",
                usage::Provider::Codex => "C",
            }
        } else {
            match p.provider {
                usage::Provider::Devin => "Devin",
                usage::Provider::Codex => "Codex",
            }
        };
        let mut nr = rect(inner_l, y, inner_l + NAME_W, y + row_h);
        draw_text(
            dc,
            st.fonts.small,
            C_DIM,
            &mut nr,
            name,
            DT_LEFT | DT_SINGLELINE | DT_VCENTER,
        );
        for (ci, win) in p.windows.iter().enumerate() {
            let cx0 = inner_l + NAME_W + CELL_GAP + ci as i32 * (each_w + CELL_GAP);
            let cx0 = cx0.min(inner_r - cell_w.min(each_w));
            let mut tr = rect(cx0, y, cx0 + each_w.min(cell_w), y + 11);
            draw_text(
                dc,
                st.fonts.small,
                cell_text_color(win.state),
                &mut tr,
                &usage_window_text(win, compact),
                DT_LEFT | DT_SINGLELINE | DT_END_ELLIPSIS,
            );
            // Bar track + fill.
            let track = rect(cx0, y + 12, cx0 + each_w.min(cell_w), y + 12 + BAR_H);
            unsafe { FillRect(dc, &track, brush) };
            if let Some(frac) = win.used_fraction {
                let fw = ((track.right - track.left) as f64 * frac.clamp(0.0, 1.0)) as i32;
                if fw > 0 {
                    let fill = rect(track.left, track.top, track.left + fw, track.bottom);
                    unsafe {
                        let fb = CreateSolidBrush(bar_color(p.provider, win.state));
                        FillRect(dc, &fill, fb);
                        DeleteObject(fb);
                    }
                }
            }
        }
    }
    unsafe {
        DeleteObject(brush);
        DeleteDC(measure_dc);
    }

    st.badge_rect = badge;
    st.hud_size = (w_px, h_px);
    st.hud_dirty = false;
}

// ---- Window state application ----------------------------------------------

fn set_visible_pair(st: &mut AppState, show: bool, hud_pos: Option<(i32, i32)>) {
    unsafe {
        if !show {
            if st.shown {
                ShowWindow(st.sprite, SW_HIDE);
                st.shown = false;
            }
            if st.hud_shown {
                ShowWindow(st.hud, SW_HIDE);
                st.hud_shown = false;
            }
            return;
        }
        if !st.shown {
            ShowWindow(st.sprite, SW_SHOWNA);
            st.shown = true;
        }
        match hud_pos {
            Some(_) => {
                if !st.hud_shown {
                    ShowWindow(st.hud, SW_SHOWNA);
                    st.hud_shown = true;
                }
            }
            None => {
                if st.hud_shown {
                    ShowWindow(st.hud, SW_HIDE);
                    st.hud_shown = false;
                }
            }
        }
    }
}

fn move_window(hwnd: HWND, pos: (i32, i32)) {
    unsafe {
        SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            pos.0,
            pos.1,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

fn apply_click_through(st: &mut AppState, on: bool) {
    for hwnd in [st.sprite, st.hud] {
        unsafe {
            let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
            let ex = if on {
                ex | WS_EX_TRANSPARENT as isize
            } else {
                ex & !(WS_EX_TRANSPARENT as isize)
            };
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex);
            SetWindowPos(
                hwnd,
                std::ptr::null_mut(),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
        }
    }
    st.ct_applied = on;
}

/// One tick: drain channels, advance the shared state machine, then
/// apply its outputs to the two surfaces (frame, HUD, visibility,
/// click-through) with no Shared borrow held.
fn on_tick(st: &mut AppState) {
    let now = Instant::now();
    let frame;
    let sprite_pos;
    let mut resized = false;
    let mut blit_key;
    let display;
    let vis;
    {
        let mut s = st.sh.borrow_mut();
        s.drain_channels(&st.rx_z, &st.rx_h, |_| true, |_| {});
        let (d, dt, v) = s.advance(now);
        display = d;
        vis = v;
        s.advance_frame_clock(dt);
        for snap in st.rx_u.try_iter() {
            if st.usage_snap != snap {
                st.usage_snap = snap;
                st.hud_dirty = true;
            }
        }
        sprite_pos = screen_pos(&s, (s.motion.x.round() as i32, s.motion.y.round() as i32));
        // Re-render the HUD whenever any drawn input changes.
        let key = format!(
            "{:?}|{}|{:?}",
            display.kind,
            s.badge_secondary(&display),
            st.usage_snap
        );
        if key != st.hud_key {
            st.hud_key = key;
            st.hud_dirty = true;
        }
        let mut force_blit = false;
        if let Some(p) = s.poll_assets() {
            resized = p.resized;
            force_blit = true;
        }
        // Frame pixels are big; copy and present them only when the visible
        // content actually changed (anim, frame index or position). The key
        // is consumed only after a real blit below — while hidden the dirty
        // state survives until the next visible present.
        blit_key = Some((
            s.current_anim.clone(),
            s.frame_index(),
            sprite_pos.0,
            sprite_pos.1,
        ));
        if force_blit || st.blit_key != blit_key {
            frame = s.current_frame();
        } else {
            frame = None;
            blit_key = None;
        }
    }
    let show = vis == hyprland::Visibility::Visible;
    if resized {
        // Profile change (V1<->V2) invalidates the surface size; the
        // next blit recreates the DIB at the new frame size.
        st.sprite_dib = None;
        st.hud_dirty = true;
    }
    if st.hud_dirty && show {
        render_hud(st, &display);
    }
    let hud_pos = if show {
        let s = st.sh.borrow();
        hud_target(&s, st.hud_size).map(|p| screen_pos(&s, p))
    } else {
        None
    };
    set_visible_pair(st, show, hud_pos);
    if show {
        if let Some(f) = &frame {
            blit_sprite(st, f, sprite_pos);
            st.blit_key = blit_key.take();
        }
        if let (Some(pos), Some(dib)) = (hud_pos, st.hud_dib.as_ref()) {
            ulw(st.hud, pos, dib);
        }
    }
    let ct = st.sh.borrow().click_through;
    if ct != st.ct_applied {
        apply_click_through(st, ct);
    }
}

// ---- Input ------------------------------------------------------------------

fn hit_opaque(st: &AppState, hwnd: HWND, x: i32, y: i32) -> bool {
    if hwnd == st.sprite {
        let s = st.sh.borrow();
        let Some(anim) = s.assets.animations.get(&s.current_anim) else {
            return false;
        };
        let (x0, y0, x1, y1) = anim.opaque_bounds;
        x >= x0 && y >= y0 && x < x1 && y < y1
    } else {
        let r = st.badge_rect;
        x >= r.left && y >= r.top && x < r.right && y < r.bottom
    }
}

/// Control menu on right-click: chat filter list, pause/click-through,
/// quit. TrackPopupMenu runs a nested message loop, so every Shared
/// borrow must be released before the call.
fn show_menu(st: &mut AppState) {
    let menu = unsafe { CreatePopupMenu() };
    if menu.is_null() {
        return;
    }
    {
        let s = st.sh.borrow();
        let pause_label = if s.motion.enabled {
            "Pause anim"
        } else {
            "Resume anim"
        };
        unsafe {
            AppendMenuW(menu, MF_STRING, IDM_PAUSE, w(pause_label).as_ptr());
            let ct_flags = if s.click_through {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING
            };
            AppendMenuW(
                menu,
                ct_flags,
                IDM_CLICKTHROUGH,
                w("Click-through").as_ptr(),
            );
        }
        let sub = unsafe { CreatePopupMenu() };
        if !sub.is_null() {
            unsafe {
                let all_flags = if s.selected_chat.is_none() {
                    MF_STRING | MF_CHECKED
                } else {
                    MF_STRING
                };
                AppendMenuW(sub, all_flags, IDM_ALL_CHATS, w("All chats").as_ptr());
            }
            if let Some(snap) = &s.snapshot {
                for (i, c) in snap.chats.iter().take(MAX_MENU_CHATS).enumerate() {
                    let title = if c.title.is_empty() {
                        "(untitled)".to_string()
                    } else {
                        format!("{} — {}", c.title, c.effective_status().label())
                    };
                    let flags = if s.selected_chat.as_deref() == Some(c.id.as_str()) {
                        MF_STRING | MF_CHECKED
                    } else {
                        MF_STRING
                    };
                    unsafe { AppendMenuW(sub, flags, IDM_CHAT_BASE + i, w(&title).as_ptr()) };
                }
            }
            unsafe { AppendMenuW(menu, MF_POPUP, sub as usize, w("Chats").as_ptr()) };
        }
        unsafe {
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            AppendMenuW(menu, MF_STRING, IDM_QUIT, w("Quit").as_ptr());
        }
    }
    let at = cursor_pos();
    unsafe {
        SetForegroundWindow(st.sprite);
        let cmd = TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN,
            at.x,
            at.y,
            0,
            st.sprite,
            std::ptr::null(),
        ) as usize;
        DestroyMenu(menu);
        PostMessageW(st.sprite, WM_NULL, 0, 0);
        match cmd {
            IDM_PAUSE => {
                let mut s = st.sh.borrow_mut();
                s.motion.enabled = !s.motion.enabled;
            }
            IDM_CLICKTHROUGH => {
                let on = {
                    let mut s = st.sh.borrow_mut();
                    s.click_through = !s.click_through;
                    s.click_through
                };
                apply_click_through(st, on);
            }
            IDM_QUIT => PostQuitMessage(0),
            IDM_ALL_CHATS => st.sh.borrow_mut().selected_chat = None,
            c if (IDM_CHAT_BASE..IDM_CHAT_BASE + MAX_MENU_CHATS).contains(&c) => {
                let i = c - IDM_CHAT_BASE;
                let id = st
                    .sh
                    .borrow()
                    .snapshot
                    .as_ref()
                    .and_then(|sn| sn.chats.get(i))
                    .map(|c| c.id.clone());
                st.sh.borrow_mut().selected_chat = id;
            }
            _ => {}
        }
    }
}

/// Triple-click chat jump: single target opens directly, otherwise a
/// compact picker at the pointer — the TrackPopupMenu analogue of the
/// GTK jump popover.
fn show_jump_menu(st: &mut AppState, targets: &[model::ChatMeta], x: i32, y: i32) {
    match targets.len() {
        0 => {}
        1 => open_chat(&targets[0].id),
        _ => {
            let menu = unsafe { CreatePopupMenu() };
            if menu.is_null() {
                return;
            }
            for (i, c) in targets.iter().take(MAX_MENU_CHATS).enumerate() {
                let title = if c.title.is_empty() {
                    "(untitled)".to_string()
                } else {
                    format!("{} — {}", c.title, c.effective_status().label())
                };
                unsafe { AppendMenuW(menu, MF_STRING, IDM_JUMP_BASE + i, w(&title).as_ptr()) };
            }
            unsafe {
                SetForegroundWindow(st.sprite);
                let cmd = TrackPopupMenu(
                    menu,
                    TPM_RETURNCMD | TPM_NONOTIFY | TPM_LEFTALIGN,
                    x,
                    y,
                    0,
                    st.sprite,
                    std::ptr::null(),
                ) as usize;
                DestroyMenu(menu);
                PostMessageW(st.sprite, WM_NULL, 0, 0);
                if (IDM_JUMP_BASE..IDM_JUMP_BASE + MAX_MENU_CHATS).contains(&cmd) {
                    if let Some(c) = targets.get(cmd - IDM_JUMP_BASE) {
                        open_chat(&c.id);
                    }
                }
            }
        }
    }
}

/// Same `zeron zeron://open/chat/<id>` handoff as the Wayland build.
/// Windows has no compositor dispatch, so raising the Zeron window is
/// left to Zeron itself.
fn open_chat(id: &str) {
    let url = format!("zeron://open/chat/{id}");
    match std::process::Command::new("zeron").arg(&url).spawn() {
        Ok(child) => launcher::reap_async(child),
        Err(e) => log::warn!("open chat spawn failed: {e}"),
    }
}

// ---- WndProc ------------------------------------------------------------------

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AppState;
    if raw.is_null() {
        if msg == WM_NCCREATE {
            let cs = lp as *const CREATESTRUCTW;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, (*cs).lpCreateParams as isize);
        }
        return DefWindowProcW(hwnd, msg, wp, lp);
    }
    let st = &mut *raw;
    match msg {
        WM_NCHITTEST => {
            if st.sh.borrow().click_through {
                return HTTRANSPARENT as LRESULT;
            }
            let (x, y) = (lp as i32 as i16 as i32, (lp as i32 >> 16) as i16 as i32);
            let mut rc: RECT = std::mem::zeroed();
            GetWindowRect(hwnd, &mut rc);
            let (lx, ly) = (x - rc.left, y - rc.top);
            if hit_opaque(st, hwnd, lx, ly) {
                HTCLIENT as LRESULT
            } else {
                HTTRANSPARENT as LRESULT
            }
        }
        WM_TIMER => {
            on_tick(st);
            0
        }
        WM_LBUTTONDOWN => {
            let s = st.sh.borrow_mut();
            if s.click_through {
                return 0;
            }
            drop(s);
            SetCapture(hwnd);
            let p = cursor_pos();
            let mut s = st.sh.borrow_mut();
            st.drag = Some(DragCtx {
                mx: s.motion.x,
                my: s.motion.y,
                cx: p.x,
                cy: p.y,
            });
            let pt = (s.motion.x.round() as i32, s.motion.y.round() as i32);
            s.behavior.press(pt);
            0
        }
        WM_MOUSEMOVE => {
            if GetCapture() != hwnd {
                return 0;
            }
            let Some(d) = st.drag.as_ref() else { return 0 };
            let (mx, my, cx, cy) = (d.mx, d.my, d.cx, d.cy);
            let p = cursor_pos();
            let (nx, ny) = (mx + (p.x - cx) as f64, my + (p.y - cy) as f64);
            let mut s = st.sh.borrow_mut();
            let geo = current_geometry(&s);
            let ab = allowed_bounds(&s);
            if let Some((bx, by)) = s.behavior.update((nx.round() as i32, ny.round() as i32)) {
                s.motion.drag_to(bx as f64, by as f64, ab);
                let actual = (s.motion.x.round() as i32, s.motion.y.round() as i32);
                s.behavior.held_drag_update(actual, geo);
                let pos = screen_pos(&s, actual);
                drop(s);
                move_window(st.sprite, pos);
            }
            0
        }
        WM_LBUTTONUP => {
            if GetCapture() == hwnd {
                ReleaseCapture();
            }
            let Some(d) = st.drag.take() else { return 0 };
            let p = cursor_pos();
            {
                let mut s = st.sh.borrow_mut();
                let nx = d.mx + (p.x - d.cx) as f64;
                let ny = d.my + (p.y - d.cy) as f64;
                let geo = current_geometry(&s);
                if let Some((tx, ty)) = s
                    .behavior
                    .release((nx.round() as i32, ny.round() as i32), geo)
                {
                    s.motion.x = tx as f64;
                    s.motion.y = ty as f64;
                    s.motion.pinned = true;
                    let pos = screen_pos(&s, (tx, ty));
                    drop(s);
                    move_window(st.sprite, pos);
                }
            }
            // The picker can reenter the message loop; the Shared borrow
            // is released above and targets are owned clones.
            if let Some(targets) = take_chat_jump_targets(&st.sh) {
                show_jump_menu(st, &targets, p.x, p.y);
            }
            0
        }
        WM_MBUTTONUP => {
            let mut s = st.sh.borrow_mut();
            s.motion.enabled = !s.motion.enabled;
            0
        }
        WM_RBUTTONUP => {
            st.sh.borrow_mut().behavior.cancel();
            show_menu(st);
            0
        }
        WM_DESTROY => {
            if hwnd == st.sprite {
                PostQuitMessage(0);
            }
            0
        }
        _ if msg == WM_CTRL => {
            let req = Box::from_raw(lp as *mut CtrlRequest);
            let mut out = String::new();
            let mut err = String::new();
            let mut code = 0;
            match cli::parse(&req.args) {
                Err(e) => {
                    code = 2;
                    err = format!("{e}\n");
                }
                Ok(parsed) => {
                    if parsed.help {
                        out = cli::HELP.to_string();
                    } else {
                        if parsed.status {
                            let d = last_display(&st.sh);
                            out = format!("{}\n", st.sh.borrow().status_line(&d));
                        }
                        {
                            let mut s = st.sh.borrow_mut();
                            s.apply_cli(&parsed);
                            let ct = s.click_through;
                            if ct != st.ct_applied {
                                drop(s);
                                apply_click_through(st, ct);
                            }
                        }
                        if parsed.has_startup_options() {
                            err = "already running: --assets/--monitor/--scale apply at launch only\n"
                                .to_string();
                        }
                        if parsed.quit {
                            PostQuitMessage(0);
                        }
                    }
                }
            }
            let _ = req.reply.send(CtrlReply { code, out, err });
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---- Single-instance control channel ----------------------------------------

/// Pipe server: one request at a time. The argv vector arrives as a JSON
/// string array and is answered with a JSON {code,out,err} — handled on
/// the UI thread through a posted WM_CTRL so Shared stays single-threaded.
fn spawn_pipe_server(hwnd: HWND) {
    // HWND is a raw pointer and not Send; the value is just an address the
    // server thread posts to — carry it as an integer across the boundary.
    let target = hwnd as usize;
    std::thread::spawn(move || loop {
        let hwnd = target as HWND;
        let name = w(PIPE_NAME);
        let h = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT,
                1,
                8192,
                8192,
                0,
                std::ptr::null(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let connected = unsafe { ConnectNamedPipe(h, std::ptr::null_mut()) };
        if connected == 0 && unsafe { GetLastError() } != ERROR_PIPE_CONNECTED {
            unsafe { CloseHandle(h) };
            continue;
        }
        let mut buf = [0u8; 8192];
        let mut got = 0u32;
        let ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok != 0 && got > 0 {
            let args: Vec<String> =
                serde_json::from_slice(&buf[..got as usize]).unwrap_or_default();
            let (tx, rx) = mpsc::channel();
            let req = Box::into_raw(Box::new(CtrlRequest { args, reply: tx }));
            let posted = unsafe { PostMessageW(hwnd, WM_CTRL, 0, req as LPARAM) };
            if posted != 0 {
                if let Ok(rep) = rx.recv_timeout(Duration::from_secs(4)) {
                    let payload = serde_json::to_vec(&rep).unwrap_or_default();
                    let mut n = 0u32;
                    unsafe {
                        WriteFile(
                            h,
                            payload.as_ptr(),
                            payload.len() as u32,
                            &mut n,
                            std::ptr::null_mut(),
                        );
                        FlushFileBuffers(h);
                    }
                }
            } else {
                // UI thread is gone; reclaim the undelivered request.
                drop(unsafe { Box::from_raw(req) });
            }
        }
        unsafe {
            DisconnectNamedPipe(h);
            CloseHandle(h);
        }
    });
}

/// Second-invocation path: forward raw argv to the running pet and relay
/// its reply. None = no server could be reached.
fn control_client(args: &[String]) -> Option<i32> {
    let name = w(PIPE_NAME);
    let payload = serde_json::to_vec(args).ok()?;
    for _ in 0..20 {
        let h = unsafe {
            CreateFileW(
                name.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                0,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            match unsafe { GetLastError() } {
                ERROR_PIPE_BUSY => {
                    unsafe { WaitNamedPipeW(name.as_ptr(), 200) };
                    continue;
                }
                // The mutex holder may still be creating its pipe — retry
                // briefly before declaring the channel dead.
                ERROR_FILE_NOT_FOUND => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                _ => return None,
            }
        }
        let mut n = 0u32;
        let wrote = unsafe {
            WriteFile(
                h,
                payload.as_ptr(),
                payload.len() as u32,
                &mut n,
                std::ptr::null_mut(),
            )
        };
        if wrote == 0 {
            unsafe { CloseHandle(h) };
            continue;
        }
        let mut buf = [0u8; 8192];
        let mut got = 0u32;
        let read_ok = unsafe {
            ReadFile(
                h,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        unsafe { CloseHandle(h) };
        if read_ok == 0 || got == 0 {
            // Server accepted then vanished — call it unreachable rather
            // than pretending the control landed.
            return None;
        }
        let rep: CtrlReply = match serde_json::from_slice(&buf[..got as usize]) {
            Ok(r) => r,
            Err(_) => {
                eprintln!("zeron-pet: malformed control reply");
                return Some(1);
            }
        };
        if !rep.out.is_empty() {
            print!("{}", rep.out);
        }
        if !rep.err.is_empty() {
            eprint!("{}", rep.err);
        }
        return Some(rep.code);
    }
    // Pipe present but never drained in ~2s.
    None
}

// ---- Entry point --------------------------------------------------------------

pub fn run(cli: Cli) -> i32 {
    // Physical-pixel coordinates for winmon rects and our window API.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };

    let raw_args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

    let mutex_name = w(MUTEX_NAME);
    let mutex = unsafe { CreateMutexW(std::ptr::null(), 1, mutex_name.as_ptr()) };
    // A failed CreateMutexW proves no peer either — proceed as the pet
    // rather than wrongly routing to the forwarding path.
    let owned = mutex.is_null() || unsafe { GetLastError() } != ERROR_ALREADY_EXISTS;
    if !owned {
        if let Some(code) = control_client(&raw_args) {
            return code;
        }
        if cli::is_control_only(&cli) {
            println!("zeron-pet: not running");
        } else {
            eprintln!("zeron-pet: control channel unreachable");
        }
        return 1;
    }
    // First instance carrying only remote-control flags has nothing to
    // control — same contract as the GTK HANDLES_COMMAND_LINE path.
    if cli::is_control_only(&cli) {
        println!("zeron-pet: not running");
        unsafe { CloseHandle(mutex) };
        return 1;
    }

    // ---- Workers --------------------------------------------------------
    let (tx_z, rx_z) = mpsc::channel::<zeron::EngineMsg>();
    let (tx_h, rx_h) = mpsc::channel::<hyprland::HyprEvent>();
    let (tx_u, rx_u) = mpsc::sync_channel::<usage::UsageSnapshot>(1);
    let stop = Arc::new(AtomicBool::new(false));
    let child_pid: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
    zeron::spawn_worker(
        zeron::McpConfig::default(),
        tx_z,
        stop.clone(),
        child_pid.clone(),
    );
    winmon::spawn_poller(tx_h, stop.clone());
    usage::spawn_cache_reader(usage::cache_path(), tx_u, stop.clone());

    // ---- Shared state (same construction as the GTK run) ------------------
    let asset_root = resolve_assets(&cli);
    let set = assets::load(&asset_root, cli.scale);
    for e in &set.errors {
        log::warn!("assets: {e}");
    }
    let scale = cli.scale as i32;
    let win_w = set.logical_w as i32 * scale;
    let win_h = set.logical_h as i32 * scale;
    let sh = Rc::new(RefCell::new(Shared {
        cli: cli.clone(),
        engine: model::Engine::new(),
        snapshot: None,
        last_counts: None,
        connected: false,
        selected_chat: None,
        monitors: Vec::new(),
        clients: Vec::new(),
        hypr_ok: false,
        monitor_name: cli.monitor.clone(),
        applied_monitor: None,
        motion: mot::Motion::new(hyprland::Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 1024,
            max_y: 1024,
        }),
        bounds: hyprland::Bounds {
            min_x: 0,
            min_y: 0,
            max_x: 1024,
            max_y: 1024,
        },
        win_w,
        win_h,
        assets: set,
        asset_root,
        click_through: false,
        demo: cli.demo.clone(),
        demo_forced_dance: cli.demo.as_deref() == Some("dance_back"),
        cycle_demo: cli.cycle_demo.then(Instant::now),
        current_anim: "idle".to_string(),
        anim_elapsed: Duration::ZERO,
        focus_elapsed: Duration::ZERO,
        ambient_elapsed: Duration::ZERO,
        joy_elapsed: Duration::ZERO,
        last_tick: Instant::now(),
        tick_count: 0,
        last_error: None,
        last_emotion: model::Emotion::Idle,
        last_kind: model::StatusKind::Unknown,
        behavior: behavior::Behavior::new(behavior::Placement::Standing),
    }));
    {
        let mut s = sh.borrow_mut();
        s.apply_cli(&cli);
    }

    // ---- Window class + two layered surfaces ------------------------------
    let hinst = unsafe { GetModuleHandleW(std::ptr::null()) };
    let class_name = w("zeron-pet-sprite");
    let hud_class_name = w("zeron-pet-hud");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: 0,
        lpfnWndProc: Some(wnd_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: std::ptr::null_mut(),
        hCursor: unsafe { LoadCursorW(std::ptr::null_mut(), IDC_ARROW) },
        hbrBackground: std::ptr::null_mut(),
        lpszMenuName: std::ptr::null(),
        lpszClassName: class_name.as_ptr(),
        hIconSm: std::ptr::null_mut(),
    };
    unsafe { RegisterClassExW(&wc) };
    let wc_hud = WNDCLASSEXW {
        lpszClassName: hud_class_name.as_ptr(),
        ..wc
    };
    unsafe { RegisterClassExW(&wc_hud) };

    // Copyable stencil: AppState must exist before CreateWindowExW — its
    // address rides lpCreateParams into WM_NCCREATE.
    let fonts = Fonts {
        primary: unsafe {
            CreateFontW(
                -13,
                0,
                0,
                0,
                FW_BOLD as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                OUT_DEFAULT_PRECIS.into(),
                CLIP_DEFAULT_PRECIS.into(),
                CLEARTYPE_QUALITY.into(),
                (FF_MODERN | FIXED_PITCH) as u32,
                w("Consolas").as_ptr(),
            )
        },
        secondary: unsafe {
            CreateFontW(
                -12,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                OUT_DEFAULT_PRECIS.into(),
                CLIP_DEFAULT_PRECIS.into(),
                CLEARTYPE_QUALITY.into(),
                (FF_MODERN | FIXED_PITCH) as u32,
                w("Consolas").as_ptr(),
            )
        },
        small: unsafe {
            CreateFontW(
                -11,
                0,
                0,
                0,
                FW_NORMAL as i32,
                0,
                0,
                0,
                DEFAULT_CHARSET.into(),
                OUT_DEFAULT_PRECIS.into(),
                CLIP_DEFAULT_PRECIS.into(),
                CLEARTYPE_QUALITY.into(),
                (FF_MODERN | FIXED_PITCH) as u32,
                w("Consolas").as_ptr(),
            )
        },
    };
    let st = Box::new(AppState {
        sh,
        sprite: std::ptr::null_mut(),
        hud: std::ptr::null_mut(),
        sprite_dib: None,
        hud_dib: None,
        hud_size: (HUD_WIDTH + 2 * HUD_SHADOW_PAD, 40),
        badge_rect: rect(0, 0, 0, 0),
        hud_dirty: true,
        hud_key: String::new(),
        usage_snap: usage::UsageSnapshot::unavailable("missing"),
        shown: false,
        hud_shown: false,
        ct_applied: false,
        blit_key: None,
        rx_z,
        rx_h,
        rx_u,
        child_pid: child_pid.clone(),
        drag: None,
        fonts,
    });
    let st_ptr = Box::into_raw(st);

    let ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
    let title = w("zeron-pet");
    let sprite = unsafe {
        CreateWindowExW(
            ex,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_POPUP,
            0,
            0,
            win_w,
            win_h,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            st_ptr as *mut c_void,
        )
    };
    let hud_title = w("zeron-pet-hud");
    let hud = unsafe {
        CreateWindowExW(
            ex,
            hud_class_name.as_ptr(),
            hud_title.as_ptr(),
            WS_POPUP,
            0,
            0,
            HUD_WIDTH + 2 * HUD_SHADOW_PAD,
            40,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinst,
            st_ptr as *mut c_void,
        )
    };
    if sprite.is_null() || hud.is_null() {
        eprintln!("zeron-pet: window creation failed");
        stop.store(true, Ordering::Relaxed);
        unsafe {
            CloseHandle(mutex);
            drop(Box::from_raw(st_ptr));
        }
        return 3;
    }
    unsafe {
        (*st_ptr).sprite = sprite;
        (*st_ptr).hud = hud;
        if (*st_ptr).ct_applied != (*st_ptr).sh.borrow().click_through {
            let ct = (*st_ptr).sh.borrow().click_through;
            apply_click_through(&mut *st_ptr, ct);
        }
        SetTimer(sprite, IDT_TICK, TICK.as_millis() as u32, None);
    }
    spawn_pipe_server(sprite);

    // ---- Message loop -----------------------------------------------------
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    loop {
        let r = unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) };
        if r <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // ---- Shutdown ---------------------------------------------------------
    unsafe {
        KillTimer(sprite, IDT_TICK);
        DestroyWindow(sprite);
        DestroyWindow(hud);
        // Reclaim AppState (windows are gone; no wnd_proc can fire).
        let st = Box::from_raw(st_ptr);
        stop.store(true, Ordering::Relaxed);
        if let Ok(g) = st.child_pid.lock() {
            if let Some(pid) = *g {
                let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
                if !h.is_null() {
                    TerminateProcess(h, 1);
                    CloseHandle(h);
                }
            }
        }
        CloseHandle(mutex);
    }
    0
}
