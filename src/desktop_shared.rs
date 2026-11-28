//! Platform-neutral desktop state: the `Shared` model, animation picking,
//! placement geometry, HUD layout and usage/status text shared by the
//! GTK layer-shell backend (desktop.rs) and the Win32 layered-window
//! backend (desktop_win.rs). No UI toolkit types appear here.

use crate::cli::Cli;
use crate::{assets, behavior, hyprland, model, motion as mot, usage, zeron};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub(crate) struct Shared {
    pub(crate) cli: Cli,
    pub(crate) engine: model::Engine,
    pub(crate) snapshot: Option<model::Snapshot>,
    pub(crate) last_counts: Option<model::Counts>,
    pub(crate) connected: bool,
    pub(crate) selected_chat: Option<String>,

    pub(crate) monitors: Vec<hyprland::MonitorInfo>,
    pub(crate) clients: Vec<hyprland::ClientInfo>,
    pub(crate) hypr_ok: bool,
    pub(crate) monitor_name: Option<String>,
    /// Connector actually pushed to gtk_layer_set_monitor.
    pub(crate) applied_monitor: Option<String>,

    pub(crate) motion: mot::Motion,
    pub(crate) bounds: hyprland::Bounds,
    pub(crate) win_w: i32,
    pub(crate) win_h: i32,

    pub(crate) assets: assets::AssetSet,
    pub(crate) asset_root: PathBuf,

    pub(crate) click_through: bool,
    pub(crate) demo: Option<String>,
    pub(crate) demo_forced_dance: bool,
    pub(crate) cycle_demo: Option<Instant>,

    pub(crate) current_anim: String,
    pub(crate) anim_elapsed: Duration,
    pub(crate) focus_elapsed: Duration,
    pub(crate) ambient_elapsed: Duration,
    pub(crate) joy_elapsed: Duration,
    pub(crate) last_tick: Instant,
    pub(crate) tick_count: u32,
    pub(crate) last_error: Option<String>,
    pub(crate) last_emotion: model::Emotion,
    pub(crate) last_kind: model::StatusKind,
    pub(crate) behavior: behavior::Behavior,
}

impl Shared {
    pub(crate) fn visibility(&self) -> hyprland::Visibility {
        if !self.hypr_ok || self.monitors.is_empty() {
            return hyprland::Visibility::HiddenUnknown;
        }
        match &self.monitor_name {
            Some(name) => hyprland::decide_visibility(&self.monitors, &self.clients, name),
            None => hyprland::Visibility::HiddenUnknown,
        }
    }

    /// The single visual-playing predicate for frame, ambient, focus,
    /// joy and reaction clocks: paused or hidden freezes them.
    /// Click-through only disables input — it is intentionally absent.
    pub(crate) fn visual_playing(&self) -> bool {
        self.motion.enabled && self.visibility() == hyprland::Visibility::Visible
    }

    pub(crate) fn status_line(&self, display: &model::DisplayState) -> String {
        let placement = self
            .behavior
            .forced_placement
            .unwrap_or(self.behavior.placement);
        let reaction = self.behavior.reaction.map(|r| r.token()).unwrap_or("none");
        format!(
            "connected={} emotion={:?} chats={} monitor={} pinned={} paused={} click_through={} assets={} last_error={}",
            display.connected,
            display.emotion,
            display.counts.shown,
            self.monitor_name.as_deref().unwrap_or("?"),
            self.motion.pinned,
            !self.motion.enabled,
            self.click_through,
            if self.assets.pending() { "pending" } else { "ok" },
            self.last_error.as_deref().unwrap_or("-"),
        ) + &format!(
            " visibility={} anim={} status={} placement={} reaction={}",
            match self.visibility() {
                hyprland::Visibility::Visible => "visible".to_string(),
                hyprland::Visibility::HiddenFullscreen { client } => {
                    format!("hidden_fullscreen({client})")
                }
                hyprland::Visibility::HiddenUnknown => "hidden_unknown".to_string(),
            },
            self.current_anim,
            display.kind.token(),
            placement.token(),
            reaction,
        )
    }
}

/// HUD content width in monitor-local logical px; height is content-sized.
pub(crate) const HUD_WIDTH: i32 = 216;

/// Clearance between the sprite window rect and the HUD rect.
pub(crate) const HUD_GAP: i32 = 8;

/// Vertical offset of side placements from the sprite's top edge.
pub(crate) const HUD_OFFSET_Y: i32 = 16;

/// Room reserved inside the HUD root so the crisp offset shadow of the
/// badge frame is not clipped by the layer surface.
pub(crate) const HUD_SHADOW_PAD: i32 = 4;

pub(crate) const TICK: Duration = Duration::from_millis(16);

pub(crate) const ASSET_POLL_EVERY: u32 = 125; // ~2s at 16ms

pub(crate) const CYCLE_EVERY: Duration = Duration::from_secs(5);

pub(crate) const DEMO_EMOTIONS: [model::Emotion; 8] = [
    model::Emotion::Idle,
    model::Emotion::Focus,
    model::Emotion::Joy,
    model::Emotion::Anger,
    model::Emotion::Sadness,
    model::Emotion::Question,
    model::Emotion::Subdued,
    model::Emotion::Disconnected,
];

pub(crate) fn parse_demo_emotion(name: &str) -> Option<model::Emotion> {
    Some(match name {
        "idle" => model::Emotion::Idle,
        "dance_back" => model::Emotion::Idle, // handled via forced dance flag
        "focus" => model::Emotion::Focus,
        "joy" => model::Emotion::Joy,
        "anger" => model::Emotion::Anger,
        "sadness" => model::Emotion::Sadness,
        "question" => model::Emotion::Question,
        "subdued" => model::Emotion::Subdued,
        "disconnected" => model::Emotion::Disconnected,
        _ => return None,
    })
}

/// Pure selector: every animation is played in a fixed window position.
/// `dance_back` is only the idle ambient back-dance or the forced demo —
/// never tied to movement (there is none) or live statuses.
pub(crate) fn pick_animation_kind(
    has_dance: bool,
    forced_dance: bool,
    emotion: model::Emotion,
    ambient_ms: u64,
    focus_ms: u64,
) -> &'static str {
    if forced_dance {
        return if has_dance { "dance_back" } else { "idle" };
    }
    if emotion == model::Emotion::Idle && has_dance {
        return if ambient_ms % 20_000 < 12_000 {
            "idle"
        } else {
            "dance_back"
        };
    }
    let phase = (focus_ms / model::FOCUS_ALTERNATE.as_millis() as u64) % 2 == 1;
    emotion.animation(phase)
}

pub(crate) fn pick_animation(shared: &Shared, display: &model::DisplayState) -> String {
    if shared.assets.version == 2 {
        return pick_animation_v2(shared, display);
    }
    pick_animation_kind(
        shared.assets.animations.contains_key("dance_back"),
        shared.demo_forced_dance,
        display.emotion,
        shared.ambient_elapsed.as_millis() as u64,
        shared.focus_elapsed.as_millis() as u64,
    )
    .to_string()
}

/// V2 transparent-inset extension past usable edges (0 for V1): the 8
/// source px of frame padding become `round(8*logical/frame*scale)-2`
/// displayed px so the visible contact can reach the 2px usable gutter.
pub(crate) fn edge_inset(s: &Shared) -> i32 {
    if s.assets.version != 2 {
        return 0;
    }
    let v = 8.0 * s.assets.logical_w as f64 / assets::V2_FRAME as f64 * s.cli.scale as f64;
    (v.round() as i32 - 2).max(0)
}

/// Window-clamp bounds: usable area extended by the V2 transparent inset
/// on left/right/bottom (top never extends). V1 returns usable bounds.
pub(crate) fn allowed_bounds(s: &Shared) -> hyprland::Bounds {
    let i = edge_inset(s);
    hyprland::Bounds {
        min_x: s.bounds.min_x - i,
        min_y: s.bounds.min_y,
        max_x: s.bounds.max_x + i,
        max_y: s.bounds.max_y + i,
    }
}

/// Per-family canonical contacts from the manifest's `_idle` entries
/// (all family contacts are consistent); placement defaults otherwise.
pub(crate) fn support_contacts(assets: &assets::AssetSet) -> behavior::SupportContacts {
    use behavior::Placement;
    let c = |name: &str, p: Placement| {
        assets
            .animations
            .get(name)
            .and_then(|a| a.contact)
            .unwrap_or_else(|| p.contact_frac())
    };
    behavior::SupportContacts {
        standing: c("standing_idle", Placement::Standing),
        floor: c("floor_idle", Placement::Floor),
        wall_left: c("wall_left_idle", Placement::WallLeft),
        wall_right: c("wall_right_idle", Placement::WallRight),
    }
}

/// Behavior geometry for the current monitor: real usable screen edges,
/// inset-extended window clamp and the *displayed* frame size. Missing
/// monitor data yields a degenerate screen that never snaps (free
/// clamped placement) instead of inventing edges.
pub(crate) fn current_geometry(s: &Shared) -> behavior::PlacementGeometry {
    let contacts = support_contacts(&s.assets);
    let frame = (s.win_w, s.win_h);
    let allowed = allowed_bounds(s);
    match s
        .monitor_name
        .as_ref()
        .and_then(|n| s.monitors.iter().find(|m| &m.name == n))
    {
        Some(mon) => behavior::placement_geometry(mon, frame, edge_inset(s), contacts),
        None => behavior::PlacementGeometry {
            screen: behavior::Bounds {
                x0: 0,
                y0: 0,
                x1: 0,
                y1: 0,
            },
            allowed,
            frame,
            contacts,
        },
    }
}

/// Placement actually shown: --pose override wins over snapped placement.
pub(crate) fn effective_placement(s: &Shared) -> behavior::Placement {
    s.behavior.forced_placement.unwrap_or(s.behavior.placement)
}

/// Resolve a V2 animation name for the effective placement, with safe
/// fallbacks so a missing entry degrades instead of going blank.
pub(crate) fn v2_anim(s: &Shared, suffix: &str) -> String {
    let p = effective_placement(s).prefix();
    let named = format!("{p}_{suffix}");
    if s.assets.animations.contains_key(&named) {
        return named;
    }
    if s.assets.animations.contains_key(suffix) {
        return suffix.to_string();
    }
    if s.assets.animations.contains_key("standing_idle") {
        return "standing_idle".to_string();
    }
    "idle".to_string()
}

/// V2 (iskra-v6) animation selection: pose-prefixed names, behavior
/// reactions first, semantic emotion underneath. Semantic status is never
/// consulted here — it lives in the badge only.
pub(crate) fn pick_animation_v2(s: &Shared, display: &model::DisplayState) -> String {
    if s.demo_forced_dance {
        return v2_anim(s, "dance_back");
    }
    if s.behavior.reaction == Some(behavior::Reaction::Dragged)
        && s.assets.animations.contains_key("dragged")
    {
        return "dragged".to_string();
    }
    if s.behavior.blush_active() {
        let n = v2_anim(s, "blush");
        if s.assets.animations.contains_key(&n) {
            return n;
        }
    }
    match display.emotion {
        model::Emotion::Idle => {
            match s.behavior.ambient_phase(
                s.ambient_elapsed,
                effective_placement(s) == behavior::Placement::Floor,
            ) {
                behavior::AmbientPhase::Dance => v2_anim(s, "dance_back"),
                behavior::AmbientPhase::Recline => {
                    if s.assets.animations.contains_key("floor_recline") {
                        "floor_recline".to_string()
                    } else {
                        v2_anim(s, "idle")
                    }
                }
                behavior::AmbientPhase::Idle => v2_anim(s, "idle"),
            }
        }
        model::Emotion::Focus => {
            let phase = (s.focus_elapsed.as_millis() as u64
                / behavior::FOCUS_ALTERNATE.as_millis() as u64)
                % 2
                == 1;
            v2_anim(
                s,
                if phase {
                    "focus_solder"
                } else {
                    "focus_laptop"
                },
            )
        }
        model::Emotion::Joy => {
            // Fresh completion celebrates at most 6s, then a calm pose
            // while the semantic label keeps reporting the ready result.
            if s.joy_elapsed < behavior::JOY_MAX {
                v2_anim(s, "joy")
            } else {
                v2_anim(s, "idle")
            }
        }
        model::Emotion::Question => v2_anim(s, "question"),
        model::Emotion::Anger => v2_anim(s, "anger"),
        _ => v2_anim(s, "sadness"),
    }
}

/// HUD placement candidates in monitor-local usable-screen coordinates:
/// RIGHT, LEFT, ABOVE, BELOW — first rect that fits fully inside the
/// usable screen and stays disjoint from the full sprite window rect
/// (with the 8px gap) wins. `sprite` is the actual window rect
/// (x, y, w, h), not artwork alpha bounds. None means "hide the HUD"
/// rather than ever covering Iskra.
pub(crate) fn hud_position(
    sprite: (i32, i32, i32, i32),
    size: (i32, i32),
    screen: behavior::Bounds,
) -> Option<(i32, i32)> {
    let (sx, sy, sw, sh) = sprite;
    let (hw, hh) = size;
    if hw <= 0 || hh <= 0 || sw <= 0 || sh <= 0 {
        return None;
    }
    // A HUD that cannot fit the usable screen at all hides instead of
    // covering artwork (also keeps the clamps below from a min>max panic).
    if hw > screen.x1 - screen.x0 || hh > screen.y1 - screen.y0 {
        return None;
    }
    let inside = |x: i32, y: i32| {
        x >= screen.x0 && y >= screen.y0 && x + hw <= screen.x1 && y + hh <= screen.y1
    };
    let disjoint = |x: i32, y: i32| x + hw <= sx || sx + sw <= x || y + hh <= sy || sy + sh <= y;
    let fits = |x: i32, y: i32| (inside(x, y) && disjoint(x, y)).then_some((x, y));

    // Sides first: x is the sprite edge ± gap; y starts a little below
    // the sprite top and clamps into the usable screen.
    let y_side = (sy + HUD_OFFSET_Y).clamp(screen.y0, screen.y1 - hh);
    fits(sx + sw + HUD_GAP, y_side)
        .or_else(|| fits(sx - HUD_GAP - hw, y_side))
        // Then above/below: y is the sprite edge ± gap; x follows the
        // sprite's left edge clamped into the usable screen.
        .or_else(|| {
            let x_over = sx.clamp(screen.x0, screen.x1 - hw);
            fits(x_over, sy - HUD_GAP - hh).or_else(|| fits(x_over, sy + sh + HUD_GAP))
        })
}

/// Monitor-local artwork envelope used for HUD disjointness: the union
/// opaque bounds of the CURRENT animation across all frames — so
/// animation/pose switches never display new art beside a stale HUD —
/// expanded 2px on every side and clipped to the sprite window. A
/// missing or empty bounds entry falls back to the full window square.
pub(crate) fn art_envelope(s: &Shared) -> (i32, i32, i32, i32) {
    let (mx, my) = (s.motion.x.round() as i32, s.motion.y.round() as i32);
    let full = (mx, my, s.win_w, s.win_h);
    let Some((x0, y0, x1, y1)) = s
        .assets
        .animations
        .get(&s.current_anim)
        .map(|a| a.opaque_bounds)
    else {
        return full;
    };
    // A degenerate stored rect (the loader's (0,0,0,0) "unknown"
    // sentinel included) means no measured envelope — full square.
    if x1 <= x0 || y1 <= y0 {
        return full;
    }
    let x0 = (x0 - 2).clamp(0, s.win_w);
    let y0 = (y0 - 2).clamp(0, s.win_h);
    let x1 = (x1 + 2).clamp(0, s.win_w);
    let y1 = (y1 + 2).clamp(0, s.win_h);
    if x1 <= x0 || y1 <= y0 {
        return full;
    }
    (mx + x0, my + y0, x1 - x0, y1 - y0)
}

/// Where the HUD belongs for the current motion/screen state, or None
/// when no non-overlapping placement exists inside the usable screen.
pub(crate) fn hud_target(s: &Shared, size: (i32, i32)) -> Option<(i32, i32)> {
    let geo = current_geometry(s);
    hud_position(art_envelope(s), size, geo.screen)
}

/// Two-phase open-chat handoff, phase one: consume the triple-click
/// request and clone its dispatch targets under a single short borrow.
/// Returning owned data lets the caller drop every RefCell guard before
/// the picker maps — mapping can synchronously cancel the gesture, and
/// that callback also borrows Shared.
pub(crate) fn take_chat_jump_targets(shared: &RefCell<Shared>) -> Option<Vec<model::ChatMeta>> {
    let mut s = shared.borrow_mut();
    if !s.behavior.take_open_chat() {
        return None;
    }
    let snap = s.snapshot.as_ref()?;
    if let Some(sel) = &s.selected_chat {
        if let Some(c) = snap.chats.iter().find(|c| &c.id == sel) {
            return Some(vec![c.clone()]);
        }
    }
    let mut chats = snap.chats.clone();
    chats.sort_by(|a, b| b.last_message_at.cmp(&a.last_message_at));
    Some(chats)
}

/// Surfaces <=140 logical px wide (legacy 96x128) have no room for the
/// full Devin/Codex labels: they get one-letter names and
/// percentage-only cells. Everything still fits inside the badge.
pub(crate) fn usage_compact(win_w: i32) -> bool {
    win_w <= 140
}

/// Column width for one usage meter: window width minus badge padding
/// (16), provider name (38 normal / 14 compact) and spacing (8), halved
/// for the two columns. Capped so wide windows stay compact.
pub(crate) fn usage_cell_width(win_w: i32) -> i32 {
    if usage_compact(win_w) {
        ((win_w - 16 - 14 - 8) / 2).clamp(20, 28)
    } else {
        ((win_w - 16 - 38 - 8) / 2).clamp(24, 66)
    }
}

/// Localized window tag for the compact gauge text.
pub(crate) fn usage_window_tag(label: &str) -> &str {
    match label {
        "24h" => "24ч",
        "7d" => "7д",
        "5h" => "5ч",
        other => other,
    }
}

/// Compact cell text: localized window tag + percent from usage.rs.
/// Percentage-only on narrow surfaces; the tags stay in the tooltip.
pub(crate) fn usage_window_text(w: &usage::WindowUsage, compact: bool) -> String {
    if compact {
        w.percent_text()
    } else {
        format!("{} {}", usage_window_tag(w.label), w.percent_text())
    }
}

/// Localized, allowlisted problem text — cache payload strings never
/// reach the UI.
#[cfg(unix)]
pub(crate) fn usage_problem_text(problem: &str) -> &'static str {
    match problem {
        "no-credentials" | "unauthorized" => "Нужен вход в Codex/Devin в Zeron",
        "missing" | "missing-windows" | "missing-timestamp" => "Нет данных",
        "multiple-accounts" => "Несколько аккаунтов: источник неоднозначен",
        "network" | "provider-error" => "Обновление Zeron не удалось",
        "cache-unavailable" => "Кэш Zeron недоступен",
        "stale" => "Данные устарели",
        "direct-auth" => "Нужен вход в Codex CLI",
        "direct-timeout" => "Codex CLI не отвечает",
        "direct-rate-limited" => "Codex CLI: лимит запросов",
        "direct-schema" => "Ответ Codex CLI не распознан",
        "direct-unavailable" | "direct-network" => "Обновление Codex CLI не удалось",
        _ => "Нет данных",
    }
}

/// Provenance of a provider's numbers: the Zeron cache is the default;
/// the Codex fallback does not switch accounts — it is always labeled
/// as the CLI's current local login when direct.
#[cfg(unix)]
pub(crate) fn usage_source_text(source: usage::UsageSource) -> &'static str {
    match source {
        usage::UsageSource::ZeronCache => "кэш Zeron",
        usage::UsageSource::CodexDirect => "Codex CLI (прямое чтение, текущий локальный вход)",
    }
}

/// Short local timestamp, or a placeholder when it is not ISO-8601 —
/// raw cache strings never reach the UI.
#[cfg(unix)]
pub(crate) fn usage_ts_text(text: &str) -> String {
    crate::timefmt::format_local_iso(text).unwrap_or_else(|| "--".to_string())
}

#[cfg(unix)]
pub(crate) fn usage_ms_text(ms: u64) -> Option<String> {
    let secs = i64::try_from(ms / 1000).ok()?;
    crate::timefmt::format_local(secs)
}

#[cfg(unix)]
pub(crate) fn usage_row_tooltip(p: &usage::ProviderUsage) -> String {
    let name = match p.provider {
        usage::Provider::Devin => "Devin",
        usage::Provider::Codex => "Codex",
    };
    let mut lines = vec![format!("Израсходовано {name}:")];
    for w in &p.windows {
        let remain = w
            .remaining_percent()
            .map(|v| format!("{v}%"))
            .unwrap_or_else(|| "—".to_string());
        let reset = w
            .resets_at
            .as_deref()
            .map(usage_ts_text)
            .unwrap_or_else(|| "—".to_string());
        let note = match w.state {
            usage::WindowState::Live => "",
            usage::WindowState::Stale => " · устарело",
            usage::WindowState::ResetPending => " · ожидает обновления",
            usage::WindowState::Unavailable => " · нет данных",
        };
        lines.push(format!(
            "{} {} · осталось {remain} · сброс {reset}{note}",
            usage_window_tag(w.label),
            w.percent_text(),
        ));
    }
    if let Some(fetched) = p.fetched_at_ms.and_then(usage_ms_text) {
        lines.push(format!("данные от {fetched}"));
    }
    lines.push(format!("Источник: {}", usage_source_text(p.source)));
    if let Some(problem) = p.problem {
        lines.push(usage_problem_text(problem).to_string());
    }
    lines.join("\n")
}

/// Select a prepared asset set under `dir`. Newest complete published set
/// wins (a manifest is only written once every animation passes), then the
/// base `assets/` set — manifest or raw source presence counts for legacy.
pub(crate) fn asset_root_in(dir: &Path) -> Option<PathBuf> {
    for version in ["v6", "v5", "v4", "v3", "v2"] {
        let candidate = dir.join("assets").join(version);
        if candidate.join("manifest.json").is_file() {
            return Some(candidate);
        }
    }
    let base = dir.join("assets");
    if base.join("manifest.json").is_file() || base.join("raw").is_dir() {
        return Some(base);
    }
    None
}

pub(crate) fn resolve_assets(cli: &Cli) -> PathBuf {
    // 1. Explicit --assets always wins.
    if let Some(p) = &cli.assets {
        return p.clone();
    }
    // 2. Walk executable ancestors up to the project root.
    if let Ok(exe) = std::env::current_exe() {
        for dir in exe.ancestors().skip(1) {
            if let Some(p) = asset_root_in(dir) {
                return p;
            }
        }
    }
    // 3. CWD fallback.
    if let Some(p) = asset_root_in(Path::new(".")) {
        return p;
    }
    PathBuf::from("assets")
}

/// Reconstruct the last display state for --status replies.
pub(crate) fn last_display(sh: &Rc<RefCell<Shared>>) -> model::DisplayState {
    let s = sh.borrow();
    let connected = s.connected
        && s.snapshot
            .as_ref()
            .is_some_and(|sn| sn.fresh(Instant::now()));
    model::DisplayState {
        emotion: s.last_emotion,
        badge: String::new(),
        connected,
        counts: s.last_counts.clone().unwrap_or_default(),
        kind: s.last_kind,
        selected_title: None,
        attention_episode: false,
        completion_episode: false,
    }
}

/// What changed after a successful asset hot-reload; the platform layer
/// reapplies its input region in any case and resizes the surface only
/// when `resized` — a profile change (V1<->V2) invalidates every sizing
/// constraint.
pub(crate) struct AssetPoll {
    pub(crate) resized: bool,
}

impl Shared {
    /// Drain the zeron engine and monitor-event channels into state.
    /// `apply_monitor` performs the platform-side output move and reports
    /// whether the connector could be bound now (false = retry next poll,
    /// e.g. GDK does not list the output yet). `on_snapshot` lets the
    /// platform refresh its chat list without a UI type leaking in here.
    pub(crate) fn drain_channels(
        &mut self,
        rx_z: &mpsc::Receiver<zeron::EngineMsg>,
        rx_h: &mpsc::Receiver<hyprland::HyprEvent>,
        mut apply_monitor: impl FnMut(&str) -> bool,
        mut on_snapshot: impl FnMut(&Shared),
    ) {
        for msg in rx_z.try_iter() {
            match msg {
                zeron::EngineMsg::Snapshot(snap) => {
                    self.snapshot = Some(snap);
                    self.connected = true;
                    on_snapshot(&*self);
                }
                zeron::EngineMsg::Down(e) => {
                    self.connected = false;
                    self.last_error = Some(e);
                }
            }
        }
        for ev in rx_h.try_iter() {
            match ev {
                hyprland::HyprEvent::State { monitors, clients } => {
                    self.hypr_ok = true;
                    if self
                        .applied_monitor
                        .as_ref()
                        .is_some_and(|name| !monitors.iter().any(|m| &m.name == name))
                    {
                        self.applied_monitor = None;
                    }
                    let mut connector_changed = false;
                    // Keep the CLI pin across hotplug. Use the last
                    // successful output for geometry until the platform
                    // surface moves us.
                    if let Some(name) = hyprland::monitor_to_apply(
                        &monitors,
                        self.cli.monitor.as_deref().or(self.monitor_name.as_deref()),
                        self.applied_monitor.as_deref(),
                    ) {
                        // No matching platform surface yet: retry next
                        // poll — do not record an apply that never happened.
                        if apply_monitor(&name) {
                            self.monitor_name = Some(name.clone());
                            self.applied_monitor = Some(name);
                            connector_changed = true;
                        }
                    }
                    // Install the fresh lists BEFORE geometry reads them;
                    // reanchor must use the new monitor's real rectangle.
                    self.monitors = monitors;
                    self.clients = clients;
                    if let Some(name) = &self.monitor_name {
                        if let Some(mon) = self.monitors.iter().find(|m| &m.name == name) {
                            let nb = hyprland::usable_bounds(mon, self.win_w, self.win_h);
                            let changed = nb.min_x != self.bounds.min_x
                                || nb.min_y != self.bounds.min_y
                                || nb.max_x != self.bounds.max_x
                                || nb.max_y != self.bounds.max_y;
                            self.bounds = nb;
                            if connector_changed {
                                // Input captured on the removed output is
                                // dead; support anchor is kept for reanchor.
                                self.behavior.cancel_all();
                            }
                            // Resize/unplug/connector re-anchors a snapped
                            // support to the new usable edge; a held press
                            // is exempt.
                            if (changed || connector_changed) && !self.behavior.is_pressing() {
                                let pos =
                                    (self.motion.x.round() as i32, self.motion.y.round() as i32);
                                let geo = current_geometry(self);
                                if let Some((tx, ty)) = self.behavior.reanchor(pos, geo) {
                                    self.motion.x = tx as f64;
                                    self.motion.y = ty as f64;
                                }
                            }
                        }
                    }
                }
                hyprland::HyprEvent::Unavailable(e) => {
                    self.hypr_ok = false;
                    self.last_error = Some(e);
                }
            }
        }
    }

    /// One tick of pure state: demo cycling, engine observation, behavior
    /// clocks and the animation pick. Returns the display state, the
    /// clamped dt for the frame clock and the visibility decision —
    /// callers use them for widget updates and surface placement.
    pub(crate) fn advance(
        &mut self,
        now: Instant,
    ) -> (model::DisplayState, Duration, hyprland::Visibility) {
        let dt = now
            .duration_since(self.last_tick)
            .min(Duration::from_millis(500));
        self.last_tick = now;

        if let Some(start) = self.cycle_demo {
            if now.duration_since(start) >= CYCLE_EVERY {
                let cur = self
                    .demo
                    .as_deref()
                    .and_then(parse_demo_emotion)
                    .unwrap_or(model::Emotion::Idle);
                let idx = DEMO_EMOTIONS.iter().position(|e| *e == cur).unwrap_or(0);
                let next = DEMO_EMOTIONS[(idx + 1) % DEMO_EMOTIONS.len()];
                // Rotating demos clears any forced reaction left behind.
                self.behavior.clear_transients();
                if next == model::Emotion::Joy {
                    // Entering the Joy demo restarts its visual budget.
                    self.joy_elapsed = Duration::ZERO;
                }
                self.demo = Some(format!("{next:?}").to_lowercase());
                self.demo_forced_dance = false;
                self.cycle_demo = Some(now);
            }
        }

        let snap = self.snapshot.clone();
        let sel = self.selected_chat.clone();
        let connected = self.connected;
        let mut display = self
            .engine
            .observe(snap.as_ref(), connected, sel.as_deref(), now);
        if !display.connected {
            if let Some(c) = &self.last_counts {
                display.counts = c.clone();
                display.badge = format!("disconnected · {}", c.summary());
            }
        }
        // A fresh awaiting-input/error episode interrupts an active blush;
        // a drag reaction is untouched. Semantic status stays live.
        if display.attention_episode {
            self.behavior.interrupt_blush();
        }
        // Reaction demos (blush/dragged) keep the live emotion; they only
        // drive the behavior layer and the demo label.
        let reaction_demo = matches!(self.demo.as_deref(), Some("blush") | Some("dragged"));
        if let Some(name) = self.demo.clone() {
            if !reaction_demo {
                let emo = parse_demo_emotion(&name).unwrap_or(model::Emotion::Idle);
                display.emotion = emo;
            }
            display.badge = format!("demo: {name} · {}", display.badge);
        }
        if reaction_demo {
            match self.demo.as_deref() {
                Some("blush") => self.behavior.force_reaction(behavior::Reaction::Blush),
                Some("dragged") => self.behavior.force_reaction(behavior::Reaction::Dragged),
                _ => {}
            }
        }

        let vis = self.visibility();
        let hidden = vis != hyprland::Visibility::Visible;
        // Stationary pet: advance() only clamps the (possibly dragged)
        // position into the allowed bounds; automatic roaming is absent.
        // V2's transparent inset lets the window rest past the usable edge.
        let ab = allowed_bounds(self);
        self.motion.advance(0.0, ab, false);
        // One visual-playing predicate for frame/ambient/focus/joy and
        // reaction clocks: paused or hidden freezes them. Click-through
        // only disables input — visuals keep running.
        let playing = self.visual_playing();
        if playing {
            self.behavior.tick(dt);
        } else {
            self.behavior.cancel();
        }
        if self.click_through {
            // Input path is empty: drop any captured press, keep visuals.
            self.behavior.cancel_capture();
        }
        if hidden {
            // Fullscreen/monitor loss: drop capture and transients but
            // keep the established support for the re-anchor on return.
            self.behavior.cancel_all();
        }
        if display.emotion == model::Emotion::Focus && playing {
            self.focus_elapsed += dt;
        } else if display.emotion != model::Emotion::Focus {
            self.focus_elapsed = Duration::ZERO;
        }
        if display.emotion == model::Emotion::Idle && playing {
            self.ambient_elapsed += dt;
        } else if display.emotion != model::Emotion::Idle {
            self.ambient_elapsed = Duration::ZERO;
        }
        // Joy budget: reset only on a NEW completion episode; demo/cycle
        // entry into Joy resets in apply_cli. Consumed budget survives
        // higher-priority interruptions and resumes on return.
        self.joy_elapsed = behavior::joy_elapsed_next(
            self.joy_elapsed,
            display.completion_episode,
            display.emotion == model::Emotion::Joy,
            playing,
            dt,
        );

        let anim = pick_animation(self, &display);
        self.last_counts = Some(display.counts.clone());
        self.last_emotion = display.emotion;
        self.last_kind = display.kind;
        if anim != self.current_anim {
            self.current_anim = anim;
            self.anim_elapsed = Duration::ZERO;
        }
        (display, dt, vis)
    }

    /// The frame clock follows the same visual-playing predicate as the
    /// ambient/focus/joy clocks: paused or hidden freezes animation.
    pub(crate) fn advance_frame_clock(&mut self, dt: Duration) {
        if self.visual_playing() {
            self.anim_elapsed += dt;
        }
    }

    /// Index into the current animation's frame list for anim_elapsed.
    /// Actual frame count from the loaded animation (V1=4, V2=8), never %4.
    pub(crate) fn frame_index(&self) -> usize {
        let anim = self.assets.animations.get(&self.current_anim);
        let frame_ms = anim.map(|a| a.frame_ms).unwrap_or(160).max(1);
        let n = anim.map(|a| a.frames.len()).unwrap_or(1).max(1);
        ((self.anim_elapsed.as_millis() as u64 / frame_ms) as usize) % n
    }

    /// The frame to show this tick, if the animation has any.
    pub(crate) fn current_frame(&self) -> Option<assets::Frame> {
        self.assets
            .animations
            .get(&self.current_anim)
            .and_then(|a| a.frames.get(self.frame_index()).cloned())
    }

    /// Secondary badge line: expendable detail — counts, selected title,
    /// a `Демо:` marker for any active visual demo and an artwork-pending
    /// note — it may ellipsize. Semantic status is never affected by
    /// demo/pose.
    pub(crate) fn badge_secondary(&self, display: &model::DisplayState) -> String {
        let mut secondary = display.kind.detail(&display.counts);
        if let Some(t) = &display.selected_title {
            secondary = if secondary.is_empty() {
                t.clone()
            } else {
                format!("{secondary} · {t}")
            };
        }
        if self.assets.pending() {
            secondary = if secondary.is_empty() {
                "artwork pending".to_string()
            } else {
                format!("{secondary} · artwork pending")
            };
        }
        if let Some(d) = &self.demo {
            secondary = if secondary.is_empty() {
                format!("Демо: {d}")
            } else {
                format!("Демо: {d} · {secondary}")
            };
        }
        secondary
    }

    /// Asset hot-reload every ASSET_POLL_EVERY ticks; the last valid set
    /// is kept on failure. Some(_) means the set was applied — callers
    /// recompute their input region, and when `resized` also resize the
    /// surface (window, picture, overlay, default size on GTK).
    pub(crate) fn poll_assets(&mut self) -> Option<AssetPoll> {
        self.tick_count += 1;
        if !self.tick_count.is_multiple_of(ASSET_POLL_EVERY) {
            return None;
        }
        let m = assets::manifest_mtime(&self.asset_root);
        if m.is_none() || m == self.assets.manifest_mtime {
            return None;
        }
        let new = assets::load(&self.asset_root, self.cli.scale);
        if new.pending() && !self.assets.pending() {
            return None;
        }
        for e in &new.errors {
            log::warn!("assets reload: {e}");
        }
        let resized = new.version != self.assets.version
            || new.logical_w != self.assets.logical_w
            || new.logical_h != self.assets.logical_h;
        if resized {
            self.win_w = new.logical_w as i32 * self.cli.scale as i32;
            self.win_h = new.logical_h as i32 * self.cli.scale as i32;
            if let Some(name) = self.monitor_name.clone() {
                if let Some(mon) = self.monitors.iter().find(|m| m.name == name) {
                    self.bounds = hyprland::usable_bounds(mon, self.win_w, self.win_h);
                }
            }
        }
        self.assets = new;
        if resized && !self.behavior.is_pressing() {
            let pos = (self.motion.x.round() as i32, self.motion.y.round() as i32);
            let geo = current_geometry(self);
            if let Some((tx, ty)) = self.behavior.reanchor(pos, geo) {
                self.motion.x = tx as f64;
                self.motion.y = ty as f64;
            }
        }
        Some(AssetPoll { resized })
    }

    /// Control-flag application shared by both backends: pause/resume,
    /// click-through, live/demo/cycle and --pose. Platform side effects
    /// (input region, margins) are reapplied by the caller — they
    /// recompute the same values when nothing changed.
    pub(crate) fn apply_cli(&mut self, cli: &Cli) {
        if cli.pause {
            self.motion.enabled = false;
        }
        if cli.resume {
            // Resume only re-enables motion; a manually pinned position
            // and its support anchor must survive pause/resume.
            self.motion.enabled = true;
        }
        if let Some(ct) = cli.click_through {
            self.click_through = ct;
        }
        if cli.live {
            self.demo = None;
            self.demo_forced_dance = false;
            self.cycle_demo = None;
            self.behavior.clear_pose();
            // --live also drops a forced transient reaction (e.g. a stuck
            // demo `dragged`) while keeping support and placement.
            self.behavior.clear_transients();
        }
        if let Some(d) = &cli.demo {
            // Switching demos clears the previous forced reaction first;
            // reaction demos re-apply below, emotion demos stay clean.
            self.behavior.clear_transients();
            self.demo = Some(d.clone());
            self.demo_forced_dance = d == "dance_back";
            self.cycle_demo = None;
            match d.as_str() {
                "blush" => self.behavior.force_reaction(behavior::Reaction::Blush),
                "dragged" => self.behavior.force_reaction(behavior::Reaction::Dragged),
                _ => {}
            }
            if d == "joy" {
                // An explicit --demo joy is a deliberate restart even when
                // Joy is already showing with a spent budget.
                self.joy_elapsed = Duration::ZERO;
            }
        }
        if cli.cycle_demo {
            self.behavior.clear_transients();
            self.demo = Some("idle".into());
            self.demo_forced_dance = false;
            self.cycle_demo = Some(Instant::now());
        }
        // --pose: display-only placement override, snapped through the same
        // contact/edge path as a real drag release.
        if let Some(p) = &cli.pose {
            if let Some(pl) = behavior::Placement::parse(p) {
                let pos = (self.motion.x.round() as i32, self.motion.y.round() as i32);
                let geo = current_geometry(self);
                if let Some((tx, ty)) = self.behavior.apply_pose(pl, pos, geo) {
                    self.motion.x = tx as f64;
                    self.motion.y = ty as f64;
                    self.motion.pinned = true;
                }
            }
        }
    }
}
