//! Cleanup list: things set aside from any page, reviewed together, then moved to the Trash.

use super::*;
use std::fs;
use std::path::Path;

#[derive(Clone)]
pub struct Staged {
    pub path: PathBuf,
    pub label: String,
    pub size: u64,
}

fn list_file() -> PathBuf {
    scan::home().join("Library/Application Support/CleanYou/cleanup-list.tsv")
}

/// The saved list, minus anything that no longer exists.
pub fn load() -> Vec<Staged> {
    fs::read_to_string(list_file())
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.splitn(3, '\t');
            let size = it.next()?.parse().ok()?;
            let label = it.next()?.to_string();
            let path = PathBuf::from(it.next()?);
            fs::symlink_metadata(&path).is_ok().then_some(Staged { path, label, size })
        })
        .collect()
}

fn save(list: &[Staged]) {
    let text: String = list
        .iter()
        .filter(|s| !s.label.contains(['\t', '\n']) && !s.path.to_string_lossy().contains('\n'))
        .map(|s| format!("{}\t{}\t{}\n", s.size, s.label, s.path.display()))
        .collect();
    if let Some(dir) = list_file().parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(list_file(), text);
}

/// Adds paths to the list. Protected paths are refused, a folder replaces anything already
/// listed inside it, and an item inside a listed folder is skipped. Returns (added, refused).
pub fn add(list: &mut Vec<Staged>, items: Vec<(PathBuf, String, u64)>) -> (usize, usize) {
    add_unless(list, items, scan::protected)
}

fn add_unless(list: &mut Vec<Staged>, items: Vec<(PathBuf, String, u64)>, protected: impl Fn(&Path) -> bool) -> (usize, usize) {
    let (mut added, mut refused) = (0, 0);
    for (path, label, size) in items {
        if protected(&path) || fs::symlink_metadata(&path).is_err() {
            refused += 1;
            continue;
        }
        if list.iter().any(|s| path.starts_with(&s.path)) {
            continue;
        }
        list.retain(|s| !s.path.starts_with(&path));
        list.push(Staged { path, label, size });
        added += 1;
    }
    list.sort_by(|a, b| b.size.cmp(&a.size));
    (added, refused)
}

/// Drops entries that were deleted (this session or outside the app). True if any went.
pub fn prune(list: &mut Vec<Staged>, removed: &HashSet<PathBuf>) -> bool {
    let n = list.len();
    list.retain(|s| !removed.contains(&s.path) && fs::symlink_metadata(&s.path).is_ok());
    list.len() != n
}

pub fn total(list: &[Staged]) -> u64 {
    list.iter().map(|s| s.size).sum()
}

fn icon_for(p: &Path) -> &'static str {
    if thumbs::is_app(p) {
        ic::PACKAGE
    } else if p.is_dir() {
        ic::FOLDER
    } else {
        ic::FILE
    }
}

enum Act {
    Unstage(PathBuf),
    Reveal(PathBuf),
    Look(PathBuf),
    Clear,
    Trash,
}

impl App {
    /// Adds items to the Cleanup list and says what happened.
    pub(crate) fn stage(&mut self, items: Vec<(PathBuf, String, u64)>) {
        let (added, refused) = add(&mut self.staged, items);
        save(&self.staged);
        let total = human(total(&self.staged));
        match (added, refused) {
            (0, 0) => self.toast("Already on the Cleanup list", true),
            (0, _) => self.toast("macOS needs that one. It can't go on the Cleanup list", false),
            (a, 0) => self.toast(format!("Added {a} to the Cleanup list · {total} set aside"), true),
            (a, r) => self.toast(format!("Added {a} · skipped {r} that macOS needs"), false),
        }
    }

    pub(crate) fn review_page(&mut self, ui: &mut Ui) {
        if prune(&mut self.staged, &self.removed) {
            save(&self.staged);
        }
        let total = total(&self.staged);
        let n = self.staged.len();
        header(
            ui,
            ic::LIST_CHECKS,
            SUCCESS,
            "Cleanup List",
            "Things you set aside from any page. Nothing is removed until you press the button.",
            |ui| {
                ui.label(RichText::new(human(total)).size(22.0).strong().color(SUCCESS));
            },
        );

        let mut act = None;
        let busy = self.del_total > 0;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}  Everything goes to the Trash, so you can put it back.", ic::SHIELD_CHECK)).color(DIM));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let t = format!("{}  Move {n} to Trash · {}", ic::TRASH, human(total));
                if primary(ui, n > 0 && !busy, t, DANGER).clicked() {
                    act = Some(Act::Trash);
                }
                if n > 0 && ui.add(Button::new(format!("{}  Clear list", ic::X)).rounding(9.0).min_size(vec2(0.0, 34.0))).clicked() {
                    act = Some(Act::Clear);
                }
            });
        });
        ui.add_space(12.0);

        let thumbs = &mut self.thumbs;
        let deleting = &self.deleting;
        Frame::none().fill(CARD).rounding(14.0).stroke(Stroke::new(1.0_f32, BORDER)).inner_margin(8.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink([false; 2]).show(ui, |ui| {
                if self.staged.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new(ic::LIST_PLUS).size(40.0).color(DIM));
                        ui.label(RichText::new("Your Cleanup list is empty").strong().color(Color32::WHITE));
                        ui.label(
                            RichText::new("Use “Add to Cleanup list” in Disk Map, Find or any category to set things aside here first.")
                                .color(DIM),
                        );
                    });
                    ui.add_space(40.0);
                }
                for s in &self.staged {
                    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
                    if !ui.is_rect_visible(rect) {
                        continue;
                    }
                    if ui.rect_contains_pointer(rect) {
                        ui.painter().rect_filled(rect, 10.0, Color32::from_white_alpha(7));
                    }
                    let cy = rect.center().y;
                    let thumb = 42.0;
                    let mut tui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(rect.left() + 8.0, cy - thumb / 2.0), vec2(thumb, thumb))));
                    thumbs::preview(&mut tui, thumbs, &s.path, thumb, icon_for(&s.path), SUCCESS);
                    let mut right = rect.right() - 6.0;
                    if deleting.contains(&s.path) {
                        let r = Rect::from_min_max(pos2(right - 110.0, rect.top()), pos2(right, rect.bottom()));
                        let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::right_to_left(Align::Center)));
                        bui.add(egui::Spinner::new().size(16.0).color(DANGER));
                        bui.label(RichText::new("Moving…").small().color(DIM));
                        right -= 116.0;
                    } else {
                        let mut btns = vec![(ic::X, "Take off the list", 0u8), (ic::FOLDER_OPEN, "Show in Finder", 1)];
                        if thumbs::previewable(&s.path) {
                            btns.push((ic::EYE, "Quick Look", 2));
                        }
                        for (icon, tip, k) in btns {
                            let r = Rect::from_center_size(pos2(right - 15.0, cy), vec2(30.0, 30.0));
                            let resp = ui.interact(r, Id::new(("staged", &s.path, k)), Sense::click());
                            paint_icon_btn(ui, r, &resp, icon, false);
                            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
                                act = Some(match k {
                                    0 => Act::Unstage(s.path.clone()),
                                    1 => Act::Reveal(s.path.clone()),
                                    _ => Act::Look(s.path.clone()),
                                });
                            }
                            right -= 32.0;
                        }
                        right -= 8.0;
                    }
                    ui.painter().text(pos2(right, cy), Align2::RIGHT_CENTER, human(s.size), FontId::proportional(14.5), Color32::WHITE);
                    right -= 92.0;
                    let x = rect.left() + 8.0 + thumb + 12.0;
                    let w = right - x - 8.0;
                    let title = truncated(ui, &disp(&s.label), 14.0, Color32::WHITE, w);
                    ui.painter().galley(pos2(x, cy - title.size().y - 1.0), title, Color32::WHITE);
                    let sub = truncated(ui, &short(&s.path), 11.5, DIM, w);
                    ui.painter().galley(pos2(x, cy + 2.0), sub, DIM);
                }
            });
        });

        match act {
            Some(Act::Unstage(p)) => {
                self.staged.retain(|s| s.path != p);
                save(&self.staged);
            }
            Some(Act::Reveal(p)) => system::reveal(&p),
            Some(Act::Look(p)) => system::quick_look(&p),
            Some(Act::Clear) => {
                self.staged.clear();
                save(&self.staged);
            }
            Some(Act::Trash) => {
                let entries = self.staged.iter().map(|s| (s.path.clone(), s.label.clone(), s.size)).collect();
                self.ask_delete_paths("Move your Cleanup list to the Trash?", entries, false);
            }
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_absorb_their_contents() {
        let d = std::env::temp_dir().join(format!("cleanyou-review-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(d.join("a/b")).unwrap();
        fs::write(d.join("a/b/f"), b"x").unwrap();
        let mut list = vec![];
        let item = |p: &str| (d.join(p), p.to_string(), 1);
        // The temp folder lives under /var, which the real rule protects; test with a stand-in.
        let add = |list: &mut Vec<Staged>, items| add_unless(list, items, |p: &Path| p.starts_with("/System"));
        add(&mut list, vec![item("a/b/f")]);
        add(&mut list, vec![item("a")]);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].path, d.join("a"));
        // Inside a listed folder: skipped.
        assert_eq!(add(&mut list, vec![item("a/b")]), (0, 0));
        // Protected and missing paths are refused.
        assert_eq!(add(&mut list, vec![(PathBuf::from("/System/Library"), "x".into(), 1), item("nope")]), (0, 2));
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn protects_system_and_roots() {
        let h = scan::home();
        for p in ["/", "/System/Library", "/usr/bin", "/Applications", "/Users", "/Volumes/Backup", "/Library/Caches", "relative"] {
            assert!(scan::protected(Path::new(p)), "{p}");
        }
        assert!(scan::protected(&h));
        assert!(scan::protected(&h.join("Library")));
        assert!(!scan::protected(&h.join("Downloads/big.zip")));
        assert!(!scan::protected(Path::new("/Applications/Foo.app")));
        assert!(!scan::protected(Path::new("/Volumes/Backup/old")));
        assert!(!scan::protected(Path::new("/usr/local/Cellar/x")));
    }
}
