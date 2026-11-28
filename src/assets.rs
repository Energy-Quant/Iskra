//! Runtime asset loading and validation.
//!
//! Manifest schema (assets/manifest.json):
//! {"version":1,"frame_width":96,"frame_height":128,"animations":{
//!   "idle":{"frames":["frames/idle/00.png",...],"frame_ms":240}, ...}}
//!
//! All paths are relative to the asset directory. Frames must be 96x128
//! RGBA PNGs with nonempty alpha; malformed or outside-root paths are
//! rejected per-animation while valid art is kept.

#[cfg(unix)]
use gdk_pixbuf::{InterpType, Pixbuf};
#[cfg(unix)]
use gtk4::gdk;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

/// One decoded, display-scaled animation frame.
/// Unix keeps it as a GDK paintable texture; Windows stores premultiplied
/// BGRA ready for `UpdateLayeredWindow` / a 32bpp DIB section.
#[cfg(unix)]
pub type Frame = gdk::Texture;

/// One decoded, display-scaled animation frame as premultiplied BGRA
/// (color channels already multiplied by alpha, byte order B,G,R,A).
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct Frame {
    pub w: i32,
    pub h: i32,
    pub bgra: std::sync::Arc<[u8]>,
}

pub const FRAME_W: u32 = 96;
pub const FRAME_H: u32 = 128;

/// V2 (iskra-v6) profile: 256px source cells presented on a 160x160 logical
/// surface — ~25% larger character than V1, smooth bilinear upscale.
pub const V2_FRAME: u32 = 256;
pub const V2_LOGICAL_W: u32 = 160;
pub const V2_LOGICAL_H: u32 = 160;
pub const V2_PROFILE: &str = "iskra-v6";
pub const V2_FRAMES_PER_ANIM: usize = 8;
/// Smooth (interpolated) packs double the frames at half frame_ms; the
/// player is count-agnostic, so both densities are valid V2.
pub const V2_FRAMES_PER_ANIM_SMOOTH: usize = 16;
pub const V2_MIN_FRAME_MS: u64 = 30;
pub const V2_MAX_FRAME_MS: u64 = 2000;

const V2_PREFIXES: [&str; 4] = ["standing", "floor", "wall_left", "wall_right"];
const V2_SUFFIXES: [&str; 9] = [
    "idle",
    "focus_laptop",
    "focus_solder",
    "question",
    "joy",
    "anger",
    "sadness",
    "blush",
    "dance_back",
];

/// All 38 mandatory V2 animation names (36 prefixed + dragged +
/// floor_recline), built at runtime since const concat cannot loop.
pub fn v2_names() -> Vec<String> {
    let mut v = Vec::with_capacity(38);
    for p in V2_PREFIXES {
        for s in V2_SUFFIXES {
            v.push(format!("{p}_{s}"));
        }
    }
    v.push("dragged".to_string());
    v.push("floor_recline".to_string());
    v
}

pub const ANIMATIONS: [&str; 8] = [
    "idle",
    "dance_back",
    "focus_laptop",
    "focus_solder",
    "joy",
    "anger",
    "sadness",
    "question",
];

pub fn frame_ms(name: &str) -> Option<u64> {
    Some(match name {
        "idle" | "sadness" => 240,
        "question" | "focus_laptop" | "focus_solder" => 180,
        "dance_back" => 180,
        "joy" | "anger" => 160,
        _ => return None,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct ManifestEntry {
    pub frames: Vec<String>,
    pub frame_ms: u64,
    /// V2 contact point in source pixels, normalized 0..1 at load.
    pub contact: Option<(u32, u32)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Manifest {
    pub animations: HashMap<String, ManifestEntry>,
    pub version: u32,
    pub profile: Option<String>,
    pub logical_w: u32,
    pub logical_h: u32,
}

fn parse_v1_manifest(v: &Value) -> Result<Manifest, String> {
    if v.get("frame_width").and_then(Value::as_u64) != Some(FRAME_W as u64)
        || v.get("frame_height").and_then(Value::as_u64) != Some(FRAME_H as u64)
    {
        return Err("manifest frame size is not 96x128".into());
    }
    let anims = v
        .get("animations")
        .and_then(Value::as_object)
        .ok_or("manifest has no animations object")?;
    let mut animations = HashMap::new();
    for (name, a) in anims {
        let frames: Vec<String> = a
            .get("frames")
            .and_then(Value::as_array)
            .map(|f| {
                f.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let frame_ms = a.get("frame_ms").and_then(Value::as_u64).unwrap_or(0);
        animations.insert(
            name.clone(),
            ManifestEntry {
                frames,
                frame_ms,
                contact: None,
            },
        );
    }
    Ok(Manifest {
        animations,
        version: 1,
        profile: None,
        logical_w: FRAME_W,
        logical_h: FRAME_H,
    })
}

/// V2 entries must carry frame_ms, playback "loop" and an in-bounds
/// contact point; unknown names, unsafe paths, non-string frame entries
/// and malformed contacts fail the whole manifest.
fn parse_v2_entry(name: &str, a: &Value) -> Result<ManifestEntry, String> {
    let raw = a
        .get("frames")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("'{name}': missing frames"))?;
    // Raw length first: a non-string element must not be silently
    // dropped into an apparently-correct count.
    if raw.len() != V2_FRAMES_PER_ANIM && raw.len() != V2_FRAMES_PER_ANIM_SMOOTH {
        return Err(format!(
            "'{name}' needs {V2_FRAMES_PER_ANIM} or {V2_FRAMES_PER_ANIM_SMOOTH} frames, got {}",
            raw.len()
        ));
    }
    let frames: Vec<String> = raw
        .iter()
        .map(|p| {
            p.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("'{name}': non-string frame path"))
        })
        .collect::<Result<_, String>>()?;
    for rel in &frames {
        if resolve_rel(Path::new("."), rel).is_none() {
            return Err(format!("'{name}': unsafe frame path '{rel}'"));
        }
    }
    let frame_ms = a.get("frame_ms").and_then(Value::as_u64).unwrap_or(0);
    if !(V2_MIN_FRAME_MS..=V2_MAX_FRAME_MS).contains(&frame_ms) {
        return Err(format!("'{name}': frame_ms {frame_ms} out of bounds"));
    }
    let playback = a.get("playback").and_then(Value::as_str).unwrap_or("");
    if playback != "loop" {
        return Err(format!("'{name}': playback '{playback}' != loop"));
    }
    let contact = a
        .get("contact")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("'{name}': missing contact [x,y]"))?;
    // Exactly two numeric coordinates, bounds-checked as u64 BEFORE the
    // u32 cast (a huge value must not wrap into range).
    if contact.len() != 2 {
        return Err(format!("'{name}': contact must have exactly 2 entries"));
    }
    let cx = contact[0]
        .as_u64()
        .ok_or_else(|| format!("'{name}': non-numeric contact x"))?;
    let cy = contact[1]
        .as_u64()
        .ok_or_else(|| format!("'{name}': non-numeric contact y"))?;
    if cx >= V2_FRAME as u64 || cy >= V2_FRAME as u64 {
        return Err(format!("'{name}': contact [{cx},{cy}] out of bounds"));
    }
    Ok(ManifestEntry {
        frames,
        frame_ms,
        contact: Some((cx as u32, cy as u32)),
    })
}

fn parse_v2_manifest(v: &Value) -> Result<Manifest, String> {
    if v.get("profile").and_then(Value::as_str) != Some(V2_PROFILE) {
        return Err(format!("manifest profile != {V2_PROFILE}"));
    }
    if v.get("frame_width").and_then(Value::as_u64) != Some(V2_FRAME as u64)
        || v.get("frame_height").and_then(Value::as_u64) != Some(V2_FRAME as u64)
    {
        return Err("manifest frame size is not 256x256".into());
    }
    if v.get("logical_width").and_then(Value::as_u64) != Some(V2_LOGICAL_W as u64)
        || v.get("logical_height").and_then(Value::as_u64) != Some(V2_LOGICAL_H as u64)
    {
        return Err("manifest logical size is not 160x160".into());
    }
    let anims = v
        .get("animations")
        .and_then(Value::as_object)
        .ok_or("manifest has no animations object")?;
    let known: std::collections::HashSet<String> = v2_names().into_iter().collect();
    let mut animations = HashMap::new();
    for (name, a) in anims {
        if !known.contains(name) {
            return Err(format!("unknown V2 animation '{name}'"));
        }
        animations.insert(name.clone(), parse_v2_entry(name, a)?);
    }
    // All 38 names mandatory: partial V2 is rejected as a whole.
    for name in v2_names() {
        if !animations.contains_key(&name) {
            return Err(format!("V2 animation '{name}' missing"));
        }
    }
    Ok(Manifest {
        animations,
        version: 2,
        profile: Some(V2_PROFILE.to_string()),
        logical_w: V2_LOGICAL_W,
        logical_h: V2_LOGICAL_H,
    })
}

pub fn parse_manifest(text: &str) -> Result<Manifest, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("manifest json: {e}"))?;
    match v.get("version").and_then(Value::as_u64) {
        Some(1) => parse_v1_manifest(&v),
        Some(2) => parse_v2_manifest(&v),
        _ => Err("manifest version not 1 or 2".into()),
    }
}

/// Resolve a manifest-relative path; rejects absolute paths, `..`
/// traversal and anything escaping the asset root.
pub fn resolve_rel(root: &Path, rel: &str) -> Option<PathBuf> {
    let p = Path::new(rel);
    if p.is_absolute() {
        return None;
    }
    let mut out = root.to_path_buf();
    for c in p.components() {
        match c {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Opaque (alpha > 0) bounding box of raw pixels; `None` when fully
/// transparent. `nch` is the channel count, alpha is the last channel.
fn alpha_bounds_px(
    w: usize,
    h: usize,
    stride: usize,
    nch: usize,
    data: &[u8],
) -> Option<(i32, i32, i32, i32)> {
    let mut min_x = w;
    let mut min_y = h;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut any = false;
    for y in 0..h {
        for x in 0..w {
            let a = data[y * stride + x * nch + 3];
            if a > 0 {
                any = true;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
    }
    any.then(|| {
        (
            min_x as i32,
            min_y as i32,
            (max_x + 1) as i32,
            (max_y + 1) as i32,
        )
    })
}

/// Opaque (alpha > 0) bounding box of a pixbuf; `None` when fully empty.
#[cfg(unix)]
pub fn alpha_bounds(pb: &Pixbuf) -> Option<(i32, i32, i32, i32)> {
    if !pb.has_alpha() {
        return Some((0, 0, pb.width(), pb.height()));
    }
    alpha_bounds_px(
        pb.width() as usize,
        pb.height() as usize,
        pb.rowstride() as usize,
        pb.n_channels() as usize,
        unsafe { pb.pixels() },
    )
}

/// Decode `path`, validate the source size and alpha channel, scale to
/// `out_w`x`out_h` (nearest for pixel art, smooth for the V2 profile) and
/// return the frame plus the *source* opaque bounds; callers rescale the
/// bounds into output pixels.
type DecodedFrame = (Frame, (i32, i32, i32, i32));

#[cfg(unix)]
fn decode_frame(
    path: &Path,
    src_w: i32,
    src_h: i32,
    out_w: i32,
    out_h: i32,
    smooth: bool,
) -> Result<DecodedFrame, String> {
    let rel = path.display();
    let pb = Pixbuf::from_file(path).map_err(|e| format!("{rel}: {e}"))?;
    if !pb.has_alpha() {
        return Err(format!("{rel}: RGB frame without alpha channel"));
    }
    if pb.width() != src_w || pb.height() != src_h {
        return Err(format!(
            "{rel}: {}x{} != {src_w}x{src_h}",
            pb.width(),
            pb.height()
        ));
    }
    let b = alpha_bounds(&pb).ok_or_else(|| format!("{rel}: fully transparent frame"))?;
    let scaled = pb
        .scale_simple(
            out_w,
            out_h,
            if smooth {
                InterpType::Bilinear
            } else {
                InterpType::Nearest
            },
        )
        .ok_or_else(|| format!("{rel}: scale failed"))?;
    Ok((gdk::Texture::for_pixbuf(&scaled), b))
}

#[cfg(windows)]
fn decode_frame(
    path: &Path,
    src_w: i32,
    src_h: i32,
    out_w: i32,
    out_h: i32,
    smooth: bool,
) -> Result<DecodedFrame, String> {
    let rel = path.display();
    let img = image::open(path).map_err(|e| format!("{rel}: {e}"))?;
    let rgba = img.to_rgba8();
    if img.color().channel_count() < 4 {
        return Err(format!("{rel}: RGB frame without alpha channel"));
    }
    if rgba.width() != src_w as u32 || rgba.height() != src_h as u32 {
        return Err(format!(
            "{rel}: {}x{} != {src_w}x{src_h}",
            rgba.width(),
            rgba.height()
        ));
    }
    let b = alpha_bounds_px(
        src_w as usize,
        src_h as usize,
        rgba.width() as usize * 4,
        4,
        rgba.as_raw(),
    )
    .ok_or_else(|| format!("{rel}: fully transparent frame"))?;
    let scaled = image::imageops::resize(
        &rgba,
        out_w.max(1) as u32,
        out_h.max(1) as u32,
        if smooth {
            image::imageops::FilterType::Triangle
        } else {
            image::imageops::FilterType::Nearest
        },
    );
    // RGBA -> premultiplied BGRA for UpdateLayeredWindow.
    let mut bgra = Vec::with_capacity(scaled.as_raw().len());
    for px in scaled.as_raw().chunks_exact(4) {
        let (r, g, bl, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
        let pm = |c: u32| ((c * a + 127) / 255) as u8;
        bgra.extend_from_slice(&[pm(bl), pm(g), pm(r), a as u8]);
    }
    Ok((
        Frame {
            w: out_w,
            h: out_h,
            bgra: bgra.into(),
        },
        b,
    ))
}

fn union_bounds(a: Option<(i32, i32, i32, i32)>, b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    match a {
        None => b,
        Some((x0, y0, x1, y1)) => (x0.min(b.0), y0.min(b.1), x1.max(b.2), y1.max(b.3)),
    }
}

#[derive(Debug)]
pub struct Animation {
    pub name: String,
    pub frame_ms: u64,
    /// Frames already upscaled (nearest for V1, smooth for V2).
    pub frames: Vec<Frame>,
    /// Union opaque bounds across frames, in displayed (scaled) pixels.
    pub opaque_bounds: (i32, i32, i32, i32),
    /// Contact point normalized 0..1 within the frame (V2 only).
    pub contact: Option<(f64, f64)>,
}

#[derive(Debug, Default)]
pub struct AssetSet {
    pub animations: HashMap<String, Animation>,
    pub errors: Vec<String>,
    pub manifest_mtime: Option<SystemTime>,
    /// Logical (pre-scale) surface size; V1 = 96x128, V2 = 160x160.
    pub logical_w: u32,
    pub logical_h: u32,
    /// Manifest version actually loaded (0 when pending/failed).
    pub version: u32,
}

impl AssetSet {
    pub fn pending(&self) -> bool {
        self.animations.is_empty()
    }
}

fn load_animation(
    root: &Path,
    name: &str,
    entry: &ManifestEntry,
    scale: u32,
) -> Result<Animation, String> {
    if !ANIMATIONS.contains(&name) {
        return Err(format!("unknown animation '{name}'"));
    }
    if entry.frames.len() != 4 {
        return Err(format!(
            "'{name}' needs exactly 4 frames, got {}",
            entry.frames.len()
        ));
    }
    let expected_ms = frame_ms(name).unwrap_or(0);
    let mut frames = Vec::with_capacity(4);
    let mut ub: Option<(i32, i32, i32, i32)> = None;
    for rel in &entry.frames {
        let path =
            resolve_rel(root, rel).ok_or_else(|| format!("'{rel}' escapes the asset root"))?;
        let scale_i = scale as i32;
        let (frame, b) = decode_frame(
            &path,
            FRAME_W as i32,
            FRAME_H as i32,
            FRAME_W as i32 * scale_i,
            FRAME_H as i32 * scale_i,
            false,
        )?;
        ub = Some(union_bounds(
            ub,
            (b.0 * scale_i, b.1 * scale_i, b.2 * scale_i, b.3 * scale_i),
        ));
        frames.push(frame);
    }
    Ok(Animation {
        name: name.to_string(),
        frame_ms: if entry.frame_ms > 0 {
            entry.frame_ms
        } else {
            expected_ms
        },
        frames,
        opaque_bounds: ub.unwrap_or((0, 0, 0, 0)),
        contact: None,
    })
}

/// V2 animation: 8 (or 16 smooth) RGBA 256x256 frames, bilinear upscale
/// to the 160x160 logical surface, per-animation contact normalized.
fn load_animation_v2(
    root: &Path,
    name: &str,
    entry: &ManifestEntry,
    scale: u32,
) -> Result<Animation, String> {
    if entry.frames.len() != V2_FRAMES_PER_ANIM && entry.frames.len() != V2_FRAMES_PER_ANIM_SMOOTH {
        return Err(format!(
            "'{name}' needs {V2_FRAMES_PER_ANIM} or {V2_FRAMES_PER_ANIM_SMOOTH} frames, got {}",
            entry.frames.len()
        ));
    }
    let out_w = V2_LOGICAL_W as i32 * scale as i32;
    let out_h = V2_LOGICAL_H as i32 * scale as i32;
    let mut frames = Vec::with_capacity(entry.frames.len());
    let mut ub: Option<(i32, i32, i32, i32)> = None;
    for rel in &entry.frames {
        let path =
            resolve_rel(root, rel).ok_or_else(|| format!("'{rel}' escapes the asset root"))?;
        let (frame, b) = decode_frame(&path, V2_FRAME as i32, V2_FRAME as i32, out_w, out_h, true)?;
        // Bounds rescale: source bounds -> output pixels.
        let sx = out_w as f64 / V2_FRAME as f64;
        let sy = out_h as f64 / V2_FRAME as f64;
        ub = Some(union_bounds(
            ub,
            (
                (b.0 as f64 * sx) as i32,
                (b.1 as f64 * sy) as i32,
                (b.2 as f64 * sx) as i32,
                (b.3 as f64 * sy) as i32,
            ),
        ));
        frames.push(frame);
    }
    let contact = entry
        .contact
        .map(|(x, y)| (x as f64 / V2_FRAME as f64, y as f64 / V2_FRAME as f64));
    Ok(Animation {
        name: name.to_string(),
        frame_ms: entry.frame_ms,
        frames,
        opaque_bounds: ub.unwrap_or((0, 0, 0, 0)),
        contact,
    })
}

/// Load and validate the whole asset directory.
/// V1: per-animation failures drop that animation only.
/// V2: all-or-nothing — any invalid/missing entry rejects the whole set so
/// a partial V6 can never activate (hot reload keeps the last valid set).
pub fn load(root: &Path, scale: u32) -> AssetSet {
    let mut set = AssetSet {
        logical_w: FRAME_W,
        logical_h: FRAME_H,
        ..AssetSet::default()
    };
    let manifest_path = root.join("manifest.json");
    set.manifest_mtime = std::fs::metadata(&manifest_path)
        .and_then(|m| m.modified())
        .ok();
    let text = match std::fs::read_to_string(&manifest_path) {
        Ok(t) => t,
        Err(e) => {
            set.errors.push(format!("manifest.json: {e}"));
            return set;
        }
    };
    let manifest = match parse_manifest(&text) {
        Ok(m) => m,
        Err(e) => {
            set.errors.push(e);
            return set;
        }
    };
    set.version = manifest.version;
    set.logical_w = manifest.logical_w;
    set.logical_h = manifest.logical_h;

    if manifest.version == 2 {
        for name in v2_names() {
            let Some(entry) = manifest.animations.get(&name) else {
                set.errors.push(format!("V2 animation '{name}' missing"));
                set.animations.clear();
                return set;
            };
            match load_animation_v2(root, &name, entry, scale) {
                Ok(a) => {
                    set.animations.insert(name.clone(), a);
                }
                Err(e) => {
                    set.errors.push(e);
                    set.animations.clear();
                    return set; // reject whole V2
                }
            }
        }
        return set;
    }

    for name in ANIMATIONS {
        match manifest.animations.get(name) {
            None => set
                .errors
                .push(format!("animation '{name}' missing from manifest")),
            Some(entry) => match load_animation(root, name, entry, scale) {
                Ok(a) => {
                    set.animations.insert(name.to_string(), a);
                }
                Err(e) => set.errors.push(e),
            },
        }
    }
    set
}

/// mtime of manifest.json, polled to detect regenerated assets.
pub fn manifest_mtime(root: &Path) -> Option<SystemTime> {
    std::fs::metadata(root.join("manifest.json"))
        .and_then(|m| m.modified())
        .ok()
}
