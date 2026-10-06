//! Previews: Quick Look thumbnails for files and real icons for apps, made off the UI thread
//! and cached in ~/Library/Caches/CleanYou/thumbs.

use super::*;
use eframe::egui::{ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Condvar;

const WORKERS: usize = 3;
/// Quick Look can hang on odd files, so every helper gets a deadline.
const TIMEOUT: Duration = Duration::from_secs(6);
const FILE_PX: u32 = 256;
const ICON_PX: u32 = 128;

const IMAGES: [&str; 12] = ["png", "jpg", "jpeg", "heic", "heif", "gif", "webp", "tiff", "tif", "bmp", "psd", "svg"];
const VIDEOS: [&str; 8] = ["mov", "mp4", "m4v", "mkv", "avi", "webm", "3gp", "mts"];
const DOCS: [&str; 4] = ["pdf", "key", "pages", "numbers"];

enum Slot {
    Pending,
    Ready(TextureHandle),
    Missing,
}

type Queue = Arc<(Mutex<Vec<PathBuf>>, Condvar)>;

pub struct Thumbs {
    ctx: Context,
    slots: HashMap<PathBuf, Slot>,
    queue: Queue,
    done: Arc<Mutex<Vec<(PathBuf, Option<ColorImage>)>>>,
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default()
}

pub fn is_app(p: &Path) -> bool {
    ext(p) == "app"
}

pub fn is_video(p: &Path) -> bool {
    VIDEOS.contains(&ext(p).as_str())
}

/// Files Quick Look can draw a real picture of, plus app bundles (their icon).
pub fn previewable(p: &Path) -> bool {
    let e = ext(p);
    e == "app" || IMAGES.contains(&e.as_str()) || VIDEOS.contains(&e.as_str()) || DOCS.contains(&e.as_str())
}

/// "/Applications/Docker.app/Contents/MacOS/x" -> "/Applications/Docker.app".
pub fn bundle_of(p: &str) -> Option<PathBuf> {
    let i = p.find(".app/").map(|i| i + 4).or_else(|| p.ends_with(".app").then_some(p.len()))?;
    Some(PathBuf::from(&p[..i]))
}

impl Thumbs {
    pub fn new(ctx: Context) -> Self {
        let queue: Queue = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
        let done = Arc::new(Mutex::new(Vec::new()));
        let dir = cache_dir();
        let _ = std::fs::create_dir_all(&dir);
        for w in 0..WORKERS {
            let (queue, done, ctx, dir) = (queue.clone(), done.clone(), ctx.clone(), dir.clone());
            std::thread::spawn(move || loop {
                let path = {
                    let (lock, cv) = &*queue;
                    let mut q = lock.lock().unwrap();
                    while q.is_empty() {
                        q = cv.wait(q).unwrap();
                    }
                    // Newest request first: that's what is on screen now.
                    q.pop().unwrap()
                };
                let img = make(&path, &dir, w);
                done.lock().unwrap().push((path, img));
                ctx.request_repaint();
            });
        }
        Thumbs { ctx, slots: HashMap::new(), queue, done }
    }

    /// Moves finished images into textures. Call once per frame.
    pub fn poll(&mut self) {
        let done: Vec<_> = std::mem::take(&mut *self.done.lock().unwrap());
        for (path, img) in done {
            let slot = match img {
                Some(img) => Slot::Ready(self.ctx.load_texture(path.to_string_lossy(), img, TextureOptions::LINEAR)),
                None => Slot::Missing,
            };
            self.slots.insert(path, slot);
        }
    }

    /// The preview for `path`, asking for it in the background the first time.
    pub fn get(&mut self, path: &Path) -> Option<&TextureHandle> {
        if !self.slots.contains_key(path) {
            self.slots.insert(path.to_path_buf(), Slot::Pending);
            let (lock, cv) = &*self.queue;
            lock.lock().unwrap().push(path.to_path_buf());
            cv.notify_one();
        }
        match self.slots.get(path) {
            Some(Slot::Ready(t)) => Some(t),
            _ => None,
        }
    }
}

fn cache_dir() -> PathBuf {
    scan::home().join("Library/Caches/CleanYou/thumbs")
}

fn key(p: &Path) -> String {
    let mut h = DefaultHasher::new();
    p.hash(&mut h);
    if let Ok(m) = std::fs::metadata(p) {
        (m.mtime(), m.size()).hash(&mut h);
    }
    format!("{:016x}.png", h.finish())
}

fn run(cmd: &mut Command) -> bool {
    let Ok(mut child) = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn() else { return false };
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(s)) => return s.success(),
            Ok(None) if start.elapsed() < TIMEOUT => std::thread::sleep(Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

fn decode(bytes: &[u8]) -> Option<ColorImage> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(ColorImage::from_rgba_unmultiplied(size, img.as_raw()))
}

/// The app's icon file: CFBundleIconFile, else any .icns, else an iOS/iPad app's PNG icon.
pub fn app_icon_file(app: &Path) -> Option<PathBuf> {
    let res = app.join("Contents/Resources");
    let named = plist::Value::from_file(app.join("Contents/Info.plist"))
        .ok()
        .and_then(|v| v.as_dictionary()?.get("CFBundleIconFile")?.as_string().map(str::to_string));
    if let Some(n) = named {
        let n = if n.ends_with(".icns") { n } else { format!("{n}.icns") };
        if res.join(&n).is_file() {
            return Some(res.join(n));
        }
    }
    if res.join("AppIcon.icns").is_file() {
        return Some(res.join("AppIcon.icns"));
    }
    if let Some(p) = std::fs::read_dir(&res).ok().and_then(|rd| rd.flatten().map(|e| e.path()).find(|p| ext(p) == "icns")) {
        return Some(p);
    }
    // iPhone/iPad apps on Apple silicon: Foo.app/Wrapper/Foo.app/AppIcon60x60@2x.png
    let inner = ["Wrapper", "WrappedBundle"]
        .iter()
        .filter_map(|w| std::fs::read_dir(app.join(w)).ok())
        .flat_map(|rd| rd.flatten().map(|e| e.path()))
        .find(|p| is_app(p))?;
    let mut pngs: Vec<(u64, PathBuf)> = std::fs::read_dir(&inner)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| ext(p) == "png" && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("AppIcon")))
        .map(|p| (std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0), p))
        .collect();
    pngs.sort();
    pngs.pop().map(|p| p.1)
}

fn make(path: &Path, dir: &Path, worker: usize) -> Option<ColorImage> {
    let out = dir.join(key(path));
    if let Ok(bytes) = std::fs::read(&out) {
        // An empty file marks "no preview possible", so we don't retry every launch.
        return if bytes.is_empty() { None } else { decode(&bytes) };
    }
    let ok = if is_app(path) {
        app_icon_file(path).is_some_and(|icon| {
            run(Command::new("/usr/bin/sips").args(["-s", "format", "png", "-Z", &ICON_PX.to_string()]).arg(&icon).arg("--out").arg(&out))
        })
    } else {
        let tmp = dir.join(format!("tmp-{worker}"));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::create_dir_all(&tmp);
        let made = run(Command::new("/usr/bin/qlmanage").args(["-t", "-s", &FILE_PX.to_string(), "-o"]).arg(&tmp).arg(path));
        let file = tmp.join(format!("{}.png", path.file_name().unwrap_or_default().to_string_lossy()));
        made && std::fs::rename(&file, &out).is_ok()
    };
    match ok.then(|| std::fs::read(&out).ok()).flatten().and_then(|b| decode(&b)) {
        Some(img) => Some(img),
        None => {
            let _ = std::fs::write(&out, b"");
            None
        }
    }
}

/// Image rect that fits `tex` inside `rect`, keeping its aspect ratio.
fn fit(rect: Rect, tex: &TextureHandle) -> Rect {
    let s = tex.size_vec2();
    let k = (rect.width() / s.x).min(rect.height() / s.y);
    Rect::from_center_size(rect.center(), s * k)
}

/// A `size`×`size` preview: the real thumbnail or app icon when there is one, otherwise a tinted icon.
/// Hovering a file thumbnail shows it bigger.
pub fn preview(ui: &mut Ui, thumbs: &mut Thumbs, path: &Path, size: f32, icon: &str, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    let visible = ui.is_rect_visible(rect);
    let tex = (visible && previewable(path)).then(|| thumbs.get(path).cloned()).flatten();
    let p = ui.painter();
    match &tex {
        Some(t) if is_app(path) => {
            egui::Image::from_texture(t).paint_at(ui, fit(rect.shrink(1.0), t));
        }
        Some(t) => {
            p.rect_filled(rect, 8.0, Color32::from_rgb(14, 14, 16));
            egui::Image::from_texture(t).rounding(7.0).paint_at(ui, fit(rect, t));
            p.rect_stroke(rect, 8.0, Stroke::new(1.0_f32, Color32::from_white_alpha(18)));
            if is_video(path) {
                let c = rect.left_bottom() + vec2(9.0, -9.0);
                p.circle_filled(c, 7.5, Color32::from_black_alpha(170));
                p.text(c + vec2(0.5, 0.0), Align2::CENTER_CENTER, ic::PLAY, FontId::proportional(8.5), Color32::WHITE);
            }
        }
        None => {
            p.rect_filled(rect, size * 0.26, color.gamma_multiply(0.16));
            p.text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(size * 0.48), color);
        }
    }
    match tex {
        Some(t) if !is_app(path) => resp.on_hover_ui(|ui| {
            let s = t.size_vec2();
            let k = (260.0 / s.x.max(s.y)).min(2.0);
            ui.add(egui::Image::from_texture(&t).fit_to_exact_size(s * k).rounding(8.0));
            ui.label(RichText::new("Click to preview · Space for Quick Look").small().color(DIM));
        }),
        _ => resp,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundles() {
        assert_eq!(bundle_of("/Applications/Docker.app/Contents/MacOS/x"), Some(PathBuf::from("/Applications/Docker.app")));
        assert_eq!(bundle_of("/Applications/Docker.app"), Some(PathBuf::from("/Applications/Docker.app")));
        assert_eq!(bundle_of("/opt/homebrew/bin/mongod"), None);
        assert!(previewable(Path::new("/x/a.MOV")) && previewable(Path::new("/x/Foo.app")) && !previewable(Path::new("/x/a.zip")));
    }
}
