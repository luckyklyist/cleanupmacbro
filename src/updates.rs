//! App updates: asks the App Store, each app's own update feed (Sparkle), or Homebrew's public
//! app catalog for the latest version. Runs only when the user presses "Check for updates", and
//! never sends the list of installed apps anywhere (the catalog is downloaded whole).

use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub enum Source {
    /// Sparkle appcast; release notes page if the feed has one.
    Sparkle { notes: Option<String> },
    AppStore { url: String },
    /// Homebrew's catalog; the app's download page.
    Catalog { homepage: Option<String> },
}

#[derive(Clone)]
pub struct Update {
    pub app: PathBuf,
    pub name: String,
    pub current: String,
    pub latest: String,
    pub source: Source,
}

#[derive(Default)]
pub struct Progress {
    pub checked: usize,
    pub total: usize,
    /// Apps that have a feed or App Store listing.
    pub with_feed: usize,
    pub found: Vec<Update>,
    pub done: bool,
}

/// Compares dotted versions numerically: "1.10" > "1.9", "2.0" == "2", "1.2b3" > "1.2".
pub fn cmp_versions(a: &str, b: &str) -> Ordering {
    let nums = |s: &str| -> Vec<u64> {
        let v: Vec<u64> = s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect();
        let mut v = v;
        while v.last() == Some(&0) {
            v.pop();
        }
        v
    };
    nums(a).cmp(&nums(b))
}

fn fetch(url: &str) -> Option<String> {
    let out = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "12", "--proto", "=https,http", "-A", "CleanYou update check"])
        .arg(url)
        .output()
        .ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Same first number, or one major release ahead. Guards against apps whose catalog version is
/// counted differently (Brave reports 154.1.96 but is listed as 1.96).
fn comparable(latest: &str, current: &str) -> bool {
    let major = |s: &str| s.split(|c: char| !c.is_ascii_digit()).find(|p| !p.is_empty()).and_then(|p| p.parse::<u64>().ok());
    match (major(latest), major(current)) {
        (Some(l), Some(c)) => l == c || l == c + 1,
        _ => false,
    }
}

/// A catalog entry: (token, version, homepage, everything else as text for matching).
struct Cask {
    version: String,
    homepage: Option<String>,
    text: String,
}

/// Homebrew casks by the .app name they install. Variants like "ghostty@tip" are left out.
fn parse_catalog(json: &str) -> HashMap<String, Vec<Cask>> {
    let mut m: HashMap<String, Vec<Cask>> = HashMap::new();
    let Ok(serde_json::Value::Array(casks)) = serde_json::from_str::<serde_json::Value>(json) else { return m };
    for c in casks {
        let token = c["token"].as_str().unwrap_or_default();
        let Some(version) = c["version"].as_str() else { continue };
        if token.contains('@') || version == "latest" {
            continue;
        }
        let apps: Vec<&str> = c["artifacts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.get("app")?.as_array())
            .flatten()
            .filter_map(|x| x.as_str())
            .collect();
        for app in apps {
            let cask = Cask {
                version: version.split(',').next().unwrap_or(version).to_string(),
                homepage: c["homepage"].as_str().map(str::to_string),
                text: format!("{} {} {}", c["url"], c["homepage"], c["uninstall"]).to_lowercase(),
            };
            m.entry(app.to_string()).or_default().push(cask);
        }
    }
    m
}

/// The catalog entry for an app: the only one with its name, or the one whose links mention
/// the vendor in its bundle id ("net.imput.helium" -> "imput").
fn pick<'a>(casks: &'a [Cask], id: &str) -> Option<&'a Cask> {
    if casks.len() == 1 {
        return casks.first();
    }
    let vendor = id.split('.').nth(1)?.to_lowercase();
    let mut hits = casks.iter().filter(|c| vendor.len() >= 3 && c.text.contains(&vendor));
    let first = hits.next()?;
    hits.next().is_none().then_some(first)
}

/// Value of `attr="…"` or `<tag>…</tag>` (Sparkle uses both styles).
fn field(item: &str, name: &str) -> Option<String> {
    let attr = format!("{name}=\"");
    if let Some(i) = item.find(&attr) {
        let rest = &item[i + attr.len()..];
        return rest.find('"').map(|j| rest[..j].trim().to_string()).filter(|s| !s.is_empty());
    }
    let open = format!("<{name}>");
    let i = item.find(&open)? + open.len();
    let j = item[i..].find('<')?;
    Some(item[i..i + j].trim().to_string()).filter(|s| !s.is_empty())
}

/// Newest (display version, build, release notes) in a Sparkle appcast, ignoring beta channels.
pub fn parse_appcast(xml: &str) -> Option<(Option<String>, Option<String>, Option<String>)> {
    let mut best: Option<(Option<String>, Option<String>, Option<String>)> = None;
    for chunk in xml.split("<item").skip(1) {
        let item = chunk.split("</item>").next().unwrap_or(chunk);
        if item.contains("<sparkle:channel>") {
            continue;
        }
        let short = field(item, "sparkle:shortVersionString");
        let build = field(item, "sparkle:version");
        if short.is_none() && build.is_none() {
            continue;
        }
        let notes = field(item, "sparkle:releaseNotesLink").or_else(|| field(item, "link")).filter(|l| l.starts_with("http"));
        let key = |v: &(Option<String>, Option<String>, Option<String>)| v.0.clone().or(v.1.clone()).unwrap_or_default();
        let cand = (short, build, notes);
        if best.as_ref().map_or(true, |b| cmp_versions(&key(&cand), &key(b)) == Ordering::Greater) {
            best = Some(cand);
        }
    }
    best
}

/// (version, store page) from an iTunes lookup JSON response.
pub fn parse_lookup(json: &str) -> Option<(String, String)> {
    let get = |k: &str| -> Option<String> {
        let pat = format!("\"{k}\":\"");
        let i = json.find(&pat)? + pat.len();
        let j = json[i..].find('"')?;
        Some(json[i..i + j].replace("\\/", "/"))
    };
    Some((get("version")?, get("trackViewUrl")?))
}

fn plist_strings(app: &Path) -> Option<plist::Dictionary> {
    plist::Value::from_file(app.join("Contents/Info.plist")).ok()?.into_dictionary()
}

/// Checks one app. Ok(None) = has a feed and is up to date; Err(()) = nothing to ask.
fn check_app(app: &Path, name: &str, catalog: &HashMap<String, Vec<Cask>>) -> Result<Option<Update>, ()> {
    let d = plist_strings(app).ok_or(())?;
    let s = |k: &str| d.get(k).and_then(|v| v.as_string()).map(str::to_string);
    let short = s("CFBundleShortVersionString").unwrap_or_default();
    let build = s("CFBundleVersion").unwrap_or_default();
    let id = s("CFBundleIdentifier").unwrap_or_default();
    let mk = |latest: String, source: Source| Update { app: app.to_path_buf(), name: name.to_string(), current: short.clone(), latest, source };

    if app.join("Contents/_MASReceipt/receipt").exists() && !id.is_empty() {
        let json = fetch(&format!("https://itunes.apple.com/lookup?bundleId={id}")).ok_or(())?;
        let (latest, url) = parse_lookup(&json).ok_or(())?;
        let newer = cmp_versions(&latest, &short) == Ordering::Greater;
        return Ok(newer.then(|| mk(latest, Source::AppStore { url })));
    }
    let file = app.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let from_catalog = || -> Result<Option<Update>, ()> {
        let cask = pick(catalog.get(&file).ok_or(())?, &id).ok_or(())?;
        if !comparable(&cask.version, &short) {
            return Err(());
        }
        let newer = cmp_versions(&cask.version, &short) == Ordering::Greater;
        Ok(newer.then(|| mk(cask.version.clone(), Source::Catalog { homepage: cask.homepage.clone() })))
    };
    let Some(feed) = s("SUFeedURL").filter(|u| u.starts_with("https://") || u.starts_with("http://")) else {
        return from_catalog();
    };
    let Some((latest_short, latest_build, notes)) = fetch(&feed).and_then(|xml| parse_appcast(&xml)) else {
        return from_catalog();
    };
    // Prefer the build number when both sides have one: display versions are sometimes stale.
    let newer = match (&latest_build, build.is_empty()) {
        (Some(b), false) => cmp_versions(b, &build) == Ordering::Greater,
        _ => latest_short.as_deref().is_some_and(|l| cmp_versions(l, &short) == Ordering::Greater),
    };
    let shown = latest_short.or(latest_build).unwrap_or_default();
    Ok(newer.then(|| mk(shown, Source::Sparkle { notes })))
}

/// Checks `apps` (path, name) a few at a time, filling `out` as results come in.
pub fn check_all(apps: Vec<(PathBuf, String)>, out: Arc<Mutex<Progress>>, ctx: eframe::egui::Context) {
    out.lock().unwrap().total = apps.len();
    let catalog = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--compressed", "--max-time", "30", "https://formulae.brew.sh/api/cask.json"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_catalog(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default();
    let queue = Mutex::new(apps);
    std::thread::scope(|s| {
        for _ in 0..6 {
            s.spawn(|| loop {
                let Some((app, name)) = queue.lock().unwrap().pop() else { break };
                let res = check_app(&app, &name, &catalog);
                let mut o = out.lock().unwrap();
                o.checked += 1;
                if let Ok(u) = res {
                    o.with_feed += 1;
                    o.found.extend(u);
                }
                drop(o);
                ctx.request_repaint();
            });
        }
    });
    let mut o = out.lock().unwrap();
    o.found.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    o.done = true;
    ctx.request_repaint();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(cmp_versions("1.10", "1.9"), Ordering::Greater);
        assert_eq!(cmp_versions("2.0", "2"), Ordering::Equal);
        assert_eq!(cmp_versions("1.2.3", "1.2.4"), Ordering::Less);
        assert_eq!(cmp_versions("125.0.6422.60", "125.0.6422.112"), Ordering::Less);
    }

    #[test]
    fn appcast() {
        let xml = r#"<rss><channel>
            <item><title>1.4</title><sparkle:version>140</sparkle:version><sparkle:shortVersionString>1.4</sparkle:shortVersionString>
              <sparkle:releaseNotesLink>https://x.com/notes/1.4</sparkle:releaseNotesLink></item>
            <item><title>1.6 beta</title><sparkle:channel>beta</sparkle:channel><sparkle:version>160</sparkle:version></item>
            <item><title>1.5</title><enclosure url="https://x.com/a.zip" sparkle:version="150" sparkle:shortVersionString="1.5"/></item>
        </channel></rss>"#;
        let (short, build, _) = parse_appcast(xml).unwrap();
        assert_eq!(short.as_deref(), Some("1.5"));
        assert_eq!(build.as_deref(), Some("150"));
        assert!(parse_appcast("<rss></rss>").is_none());
    }

    #[test]
    fn catalog() {
        let json = r#"[
          {"token":"ghostty","version":"1.3.1","homepage":"https://ghostty.org","artifacts":[{"app":["Ghostty.app"]}]},
          {"token":"ghostty@tip","version":"18079,abc","artifacts":[{"app":["Ghostty.app"]}]},
          {"token":"helium","version":"1.0.0","url":"https://other.dev/h.dmg","artifacts":[{"app":["Helium.app"]}]},
          {"token":"helium-browser","version":"0.18.3.1,x","url":"https://github.com/imputnet/helium/h.dmg","artifacts":[{"uninstall":{}},{"app":["Helium.app"]}]},
          {"token":"weird","version":"latest","artifacts":[{"app":["Weird.app"]}]}
        ]"#;
        let m = parse_catalog(json);
        assert_eq!(m["Ghostty.app"].len(), 1);
        assert_eq!(pick(&m["Ghostty.app"], "com.mitchellh.ghostty").unwrap().version, "1.3.1");
        assert_eq!(pick(&m["Helium.app"], "net.imput.helium").unwrap().version, "0.18.3.1");
        assert!(pick(&m["Helium.app"], "com.unknown.helium").is_none());
        assert!(!m.contains_key("Weird.app"));
        assert!(comparable("3.0.24", "3.0.23") && comparable("2.6.3", "1.104.31"));
        assert!(!comparable("1.96.61.0", "154.1.96.61"));
    }

    #[test]
    fn lookup() {
        let json = r#"{"resultCount":1,"results":[{"trackViewUrl":"https:\/\/apps.apple.com\/us\/app\/x\/id1?mt=12","version":"3.2.1"}]}"#;
        assert_eq!(parse_lookup(json), Some(("3.2.1".into(), "https://apps.apple.com/us/app/x/id1?mt=12".into())));
        assert!(parse_lookup(r#"{"resultCount":0,"results":[]}"#).is_none());
    }
}


pub const REPO: &str = "luckyklyist/cleanupmacbro";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Clean You's own update state, shown next to the version in the sidebar.
#[derive(Clone, PartialEq)]
pub enum SelfUpdate {
    Checking,
    UpToDate,
    /// Newer version and the page to download it from.
    Available(String, String),
    Failed,
}

/// Asks GitHub for the latest Clean You release. Only runs when the user presses the button.
pub fn check_self(out: Arc<Mutex<SelfUpdate>>, ctx: eframe::egui::Context) {
    std::thread::spawn(move || {
        let res = fetch(&format!("https://api.github.com/repos/{REPO}/releases/latest"))
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| {
                let tag = v["tag_name"].as_str()?.trim_start_matches('v').to_string();
                let url = v["html_url"].as_str()?.to_string();
                Some((tag, url))
            });
        *out.lock().unwrap() = match res {
            Some((tag, url)) if cmp_versions(&tag, VERSION) == Ordering::Greater => SelfUpdate::Available(tag, url),
            Some(_) => SelfUpdate::UpToDate,
            None => SelfUpdate::Failed,
        };
        ctx.request_repaint();
    });
}
