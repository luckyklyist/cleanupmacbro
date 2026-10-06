//! Photo compressor: re-encodes photos as HEIC or JPEG with macOS's built-in `sips`.

use crate::scan::{remove, unique};
use crate::video::Quality;
use eframe::egui;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

pub const EXTS: [&str; 9] = ["jpg", "jpeg", "png", "heic", "heif", "tif", "tiff", "bmp", "webp"];
/// Results that save less than this share are thrown away.
const MIN_SAVING: f64 = 0.10;

#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Heic,
    Jpeg,
}

#[derive(Clone, Copy)]
pub struct Opts {
    pub quality: Quality,
    pub format: Format,
    pub replace: bool,
}

#[derive(Clone)]
pub enum Status {
    Queued,
    Running,
    Done(PathBuf, u64),
    /// Left as it was (reason).
    Skipped(String),
    Failed(String),
}

#[derive(Clone)]
pub struct Job {
    pub path: PathBuf,
    pub size: u64,
    pub status: Status,
}

fn ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn is_photo(p: &Path) -> bool {
    EXTS.contains(&ext(p).as_str()) && p.is_file()
}

pub fn new_job(path: PathBuf) -> Job {
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Job { path, size, status: Status::Queued }
}

/// Photos of at least 300 KB inside `dir`, skipping hidden folders and library bundles
/// (the Photos library manages its own files).
pub fn collect(dir: &Path) -> Vec<PathBuf> {
    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            let n = e.file_name().to_string_lossy();
            e.depth() == 0 || !(n.starts_with('.') || n.ends_with(".photoslibrary") || n.ends_with(".app") || n.ends_with(".photolibrary"))
        })
        .flatten()
        .filter(|e| e.file_type().is_file() && EXTS.contains(&ext(e.path()).as_str()))
        .filter(|e| e.metadata().is_ok_and(|m| m.len() >= 300_000))
        .map(|e| e.into_path())
        .collect()
}

fn percent(q: Quality) -> &'static str {
    match q {
        Quality::Smallest => "45",
        Quality::Balanced => "65",
        Quality::Best => "80",
    }
}

fn has_alpha(p: &Path) -> bool {
    Command::new("/usr/bin/sips")
        .args(["-g", "hasAlpha"])
        .arg(p)
        .output()
        .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("hasAlpha: yes"))
}

fn process(job: &Job, opts: Opts) -> Status {
    let p = &job.path;
    let (fmt, out_ext) = match opts.format {
        Format::Heic => ("heic", "heic"),
        Format::Jpeg => ("jpeg", "jpg"),
    };
    if opts.format == Format::Jpeg && has_alpha(p) {
        return Status::Skipped("Has transparency. Choose HEIC to keep it".into());
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let dir = p.parent().unwrap_or(Path::new("/"));
    let out = unique(dir, &format!("{stem} (compressed)"), out_ext);
    let res = Command::new("/usr/bin/sips")
        .args(["-s", "format", fmt, "-s", "formatOptions", percent(opts.quality)])
        .arg(p)
        .arg("--out")
        .arg(&out)
        .output();
    match res {
        Ok(o) if o.status.success() && out.exists() => {}
        Ok(o) => {
            let _ = std::fs::remove_file(&out);
            let err = String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("sips couldn't read this photo").trim().to_string();
            return Status::Failed(err);
        }
        Err(e) => return Status::Failed(e.to_string()),
    }
    let new_size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(u64::MAX);
    if new_size as f64 > job.size as f64 * (1.0 - MIN_SAVING) {
        let _ = std::fs::remove_file(&out);
        return Status::Skipped("Already small".into());
    }
    // Keep the original dates, so the photo stays in place when sorted by date.
    let _ = Command::new("/usr/bin/touch").arg("-r").arg(p).arg(&out).status();
    if !opts.replace {
        return Status::Done(out, new_size);
    }
    if let Err(e) = remove(p, false) {
        return Status::Failed(format!("Saved a smaller copy, but couldn't move the original to the Trash: {e}"));
    }
    let target = unique(dir, &stem, out_ext);
    match std::fs::rename(&out, &target) {
        Ok(()) => Status::Done(target, new_size),
        Err(_) => Status::Done(out, new_size),
    }
}

/// Works through queued jobs with a few threads.
pub fn run_queue(jobs: Arc<Mutex<Vec<Job>>>, opts: Opts, ctx: egui::Context) {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2).clamp(2, 4);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| loop {
                let next = {
                    let mut j = jobs.lock().unwrap();
                    j.iter().position(|x| matches!(x.status, Status::Queued)).map(|i| {
                        j[i].status = Status::Running;
                        (i, j[i].clone())
                    })
                };
                let Some((i, job)) = next else { break };
                ctx.request_repaint();
                let status = process(&job, opts);
                if let Some(j) = jobs.lock().unwrap().get_mut(i).filter(|j| j.path == job.path) {
                    j.status = status;
                }
                ctx.request_repaint();
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cleanyou-photos-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A big, noisy-but-smooth PNG that compresses well as HEIC.
    fn make_png(p: &Path) {
        let (w, h) = (1200u32, 900u32);
        let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / w) as u8, (y * 255 / h) as u8, ((x ^ y) % 256) as u8]));
        img.save(p).unwrap();
    }

    #[test]
    fn compresses_to_heic_and_keeps_original() {
        let d = tmp();
        let src = d.join("pic.png");
        make_png(&src);
        let job = new_job(src.clone());
        let opts = Opts { quality: Quality::Balanced, format: Format::Heic, replace: false };
        match process(&job, opts) {
            Status::Done(out, size) => {
                assert_eq!(out, d.join("pic (compressed).heic"));
                assert!(size < job.size);
                assert!(src.exists());
            }
            Status::Skipped(s) | Status::Failed(s) => panic!("{s}"),
            _ => unreachable!(),
        }
        // A tiny output that saves nothing is skipped.
        let small = d.join("pic (compressed).heic");
        assert!(matches!(process(&new_job(small), opts), Status::Skipped(_)));
        assert_eq!(collect(&d).len(), 1, "only the PNG is 300 KB or more");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
