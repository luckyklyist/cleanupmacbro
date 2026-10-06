use crate::scan::{remove, unique};
use eframe::egui;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, PartialEq)]
pub enum Mode {
    Compress,
    Restore,
}

#[derive(Clone)]
pub enum Status {
    Queued,
    Running(f32),
    Done(PathBuf, u64),
    Failed(String),
}

#[derive(Clone)]
pub struct Job {
    pub path: PathBuf,
    pub size: u64,
    pub mode: Mode,
    pub status: Status,
    /// Original width, height, bitrate saved inside files we compressed.
    pub orig: Option<(u32, u32, u64)>,
}

#[derive(Clone, Copy, PartialEq)]
pub enum Quality {
    Smallest,
    Balanced,
    Best,
}

impl Quality {
    fn q(self) -> &'static str {
        match self {
            Quality::Smallest => "40",
            Quality::Balanced => "52",
            Quality::Best => "62",
        }
    }
}

#[derive(Clone, Copy)]
pub struct Opts {
    pub quality: Quality,
    pub downscale: bool,
    pub replace: bool,
}

pub fn tool(name: &str) -> Option<PathBuf> {
    ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.exists())
}

struct Info {
    dur: f64,
    w: u32,
    h: u32,
    br: u64,
    tag: Option<(u32, u32, u64)>,
}

fn probe(p: &Path) -> Option<Info> {
    let out = Command::new(tool("ffprobe")?)
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height:format=duration,bit_rate:format_tags=comment"])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(p)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut i = Info { dur: 0.0, w: 0, h: 0, br: 0, tag: None };
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "width" => i.w = v.parse().unwrap_or(0),
            "height" => i.h = v.parse().unwrap_or(0),
            "duration" => i.dur = v.parse().unwrap_or(0.0),
            "bit_rate" => i.br = v.parse().unwrap_or(0),
            "TAG:comment" => i.tag = parse_tag(v),
            _ => {}
        }
    }
    (i.w > 0).then_some(i)
}

/// Tag format written on compress: "cleanyou:1920x1080@8000000"
fn parse_tag(s: &str) -> Option<(u32, u32, u64)> {
    let rest = s.strip_prefix("cleanyou:")?;
    let (dims, br) = rest.split_once('@')?;
    let (w, h) = dims.split_once('x')?;
    Some((w.parse().ok()?, h.parse().ok()?, br.parse().ok()?))
}

pub fn new_job(path: PathBuf) -> Job {
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let orig = probe(&path).and_then(|i| i.tag);
    let mode = if orig.is_some() { Mode::Restore } else { Mode::Compress };
    Job { path, size, mode, status: Status::Queued, orig }
}

pub fn run_queue(jobs: Arc<Mutex<Vec<Job>>>, opts: Opts, ctx: egui::Context) {
    loop {
        let next = {
            let mut j = jobs.lock().unwrap();
            j.iter().position(|x| matches!(x.status, Status::Queued)).map(|i| {
                j[i].status = Status::Running(0.0);
                (i, j[i].clone())
            })
        };
        let Some((i, job)) = next else { break };
        let res = process(&job, opts, |p| {
            jobs.lock().unwrap()[i].status = Status::Running(p);
            ctx.request_repaint();
        });
        jobs.lock().unwrap()[i].status = match res {
            Ok((p, s)) => Status::Done(p, s),
            Err(e) => Status::Failed(e),
        };
        ctx.request_repaint();
    }
}

fn process(job: &Job, opts: Opts, progress: impl Fn(f32)) -> Result<(PathBuf, u64), String> {
    let ff = tool("ffmpeg").ok_or("ffmpeg not found. Install it with: brew install ffmpeg")?;
    let info = probe(&job.path).ok_or("Could not read this video")?;
    let stem = job.path.file_stem().unwrap_or_default().to_string_lossy().to_string();
    let dir = job.path.parent().unwrap_or(Path::new("/"));

    let mut args: Vec<String> = ["-y", "-hide_banner", "-loglevel", "error", "-nostats", "-progress", "pipe:1", "-i"]
        .map(String::from)
        .to_vec();
    args.push(job.path.to_string_lossy().into());

    let out = match job.mode {
        Mode::Compress => {
            let out = unique(dir, &format!("{stem} (compressed)"), "mov");
            if opts.downscale && info.w.min(info.h) > 1080 {
                let vf = if info.w >= info.h { "scale=-2:1080" } else { "scale=1080:-2" };
                args.extend(["-vf".into(), vf.into()]);
            }
            let tag = format!("comment=cleanyou:{}x{}@{}", info.w, info.h, info.br);
            args.extend(
                ["-map_metadata", "0", "-c:v", "hevc_videotoolbox", "-q:v", opts.quality.q(), "-tag:v", "hvc1"]
                    .map(String::from),
            );
            args.extend(["-c:a", "aac", "-b:a", "128k", "-movflags", "+faststart", "-metadata"].map(String::from));
            args.push(tag);
            out
        }
        Mode::Restore => {
            let (w, h, br) = job.orig.or(info.tag).ok_or("This video wasn't compressed by Clean You")?;
            let base = stem.trim_end_matches(" (compressed)");
            let out = unique(dir, &format!("{base} (restored)"), "mov");
            args.extend([
                "-vf".into(),
                format!("scale={w}:{h}:flags=lanczos"),
                "-c:v".into(),
                "h264_videotoolbox".into(),
                "-b:v".into(),
                br.max(1_000_000).to_string(),
            ]);
            args.extend(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"].map(String::from));
            out
        }
    };
    args.push(out.to_string_lossy().into());

    let mut child = Command::new(ff)
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    for line in BufReader::new(child.stdout.take().unwrap()).lines().map_while(Result::ok) {
        if let Some(us) = line.strip_prefix("out_time_us=").or(line.strip_prefix("out_time_ms=")) {
            if let (Ok(us), true) = (us.parse::<f64>(), info.dur > 0.0) {
                progress((us / 1e6 / info.dur).clamp(0.0, 1.0) as f32);
            }
        }
    }
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    let err = err_reader.join().unwrap_or_default();
    if !ok {
        let _ = std::fs::remove_file(&out);
        return Err(err.lines().last().unwrap_or("ffmpeg failed").to_string());
    }

    let new_size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    if job.mode == Mode::Compress {
        if new_size >= job.size {
            let _ = std::fs::remove_file(&out);
            return Err("Already well compressed; kept the original".into());
        }
        if opts.replace {
            remove(&job.path, false)?;
            let target = dir.join(format!("{stem}.mov"));
            if !target.exists() && std::fs::rename(&out, &target).is_ok() {
                return Ok((target, new_size));
            }
        }
    }
    Ok((out, new_size))
}
