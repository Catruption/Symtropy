use std::fs;
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use serde::{Deserialize, Serialize};
use sysinfo::Disks;

#[derive(Serialize, Deserialize, Default)]
pub struct Bookmarks {
    pub paths: Vec<PathBuf>,
}

impl Bookmarks {
    pub fn load() -> Self {
        let path = dirs::config_dir().unwrap_or_default().join("symtropy_bookmarks.json");
        if let Ok(text) = fs::read_to_string(path) {
            serde_json::from_str(&text).unwrap_or_default()
        } else { Self::default() }
    }
    pub fn save(&self) {
        let path = dirs::config_dir().unwrap_or_default().join("symtropy_bookmarks.json");
        let _ = fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

#[derive(Clone, PartialEq)]
pub enum ViewMode { List, Gallery }

pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

pub struct FileManagerState {
    pub current_dir: PathBuf,
    pub history: Vec<PathBuf>,
    pub history_idx: usize,
    pub entries: Vec<FileEntry>,
    pub selected: HashSet<PathBuf>,
    pub view_mode: ViewMode,
    pub show_hidden: bool,
    pub search_query: String,
    pub mounts: Vec<PathBuf>,
    pub bookmarks: Bookmarks,
}

impl FileManagerState {
    pub fn new(initial_dir: PathBuf) -> Self {
        let mut s = Self {
            current_dir: initial_dir,
            history: Vec::new(),
            history_idx: 0,
            entries: Vec::new(),
            selected: HashSet::new(),
            view_mode: ViewMode::List,
            show_hidden: false,
            search_query: String::new(),
            mounts: Vec::new(),
            bookmarks: Bookmarks::load(),
        };
        s.refresh_mounts();
        s.navigate_to(s.current_dir.clone());
        s
    }

    pub fn refresh_mounts(&mut self) {
        let disks = Disks::new_with_refreshed_list();
        self.mounts = disks.iter().map(|d| d.mount_point().to_path_buf()).collect();
    }

    pub fn navigate_to(&mut self, path: PathBuf) {
        if !path.exists() || !path.is_dir() { return; }
        self.current_dir = path.clone();
        self.selected.clear();
        self.search_query.clear();

        if self.history.is_empty() || self.history[self.history_idx] != path {
            self.history.truncate(self.history_idx + 1);
            self.history.push(path);
            self.history_idx = self.history.len() - 1;
        }
        self.refresh();
    }

    pub fn go_back(&mut self) {
        if self.history_idx > 0 {
            self.history_idx -= 1;
            self.current_dir = self.history[self.history_idx].clone();
            self.refresh();
        }
    }

    pub fn go_forward(&mut self) {
        if self.history_idx + 1 < self.history.len() {
            self.history_idx += 1;
            self.current_dir = self.history[self.history_idx].clone();
            self.refresh();
        }
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        if let Ok(rd) = fs::read_dir(&self.current_dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // This line hides the new dotfile sidecars automatically
                if !self.show_hidden && name.starts_with('.') { continue; }

                self.entries.push(FileEntry {
                    name,
                    path: entry.path(),
                                  is_dir: entry.path().is_dir(),
                });
            }
        }
        self.entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase())));
    }

    pub fn search_recursive(&mut self) {
        if self.search_query.is_empty() { self.refresh(); return; }
        let query = self.search_query.to_lowercase();
        self.entries.clear();

        let mut stack = vec![self.current_dir.clone()];
        while let Some(dir) = stack.pop() {
            if let Ok(rd) = fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !self.show_hidden && name.starts_with('.') { continue; }

                    if name.to_lowercase().contains(&query) {
                        self.entries.push(FileEntry {
                            name: name.clone(),
                                          path: entry.path(),
                                          is_dir: entry.path().is_dir(),
                        });
                    }
                    if entry.path().is_dir() && !entry.path().is_symlink() { stack.push(entry.path()); }
                }
            }
        }
        self.entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    }

    pub fn open_terminal(&self) {
        for t in ["konsole", "gnome-terminal", "alacritty", "kitty", "xterm"] {
            if std::process::Command::new(t).current_dir(&self.current_dir).spawn().is_ok() { return; }
        }
    }

    pub fn open_smart(&self, path: &Path) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if ["txt", "md", "rs", "toml", "json", "py", "js", "ts", "c", "cpp", "h", "sh", "cfg", "ini"].contains(&ext.as_str()) {
            if std::process::Command::new("kate").arg(path).spawn().is_ok() { return; }
        }
        let _ = open::that(path);
    }

    pub fn trash_selected(&mut self) {
        for p in &self.selected { let _ = trash::delete(p); }
        self.refresh();
    }

    pub fn add_bookmark(&mut self) {
        if !self.bookmarks.paths.contains(&self.current_dir) {
            self.bookmarks.paths.push(self.current_dir.clone());
            self.bookmarks.save();
        }
    }
}
