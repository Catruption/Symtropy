use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use sysinfo::Disks;

#[derive(Serialize, Deserialize, Default)]
pub struct Bookmarks {
    pub paths: Vec<PathBuf>,
}

impl Bookmarks {
    pub fn load() -> Self {
        let path = crate::paths::bookmarks_path();
        if let Ok(text) = fs::read_to_string(path) {
            serde_json::from_str(&text).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self) {
        let path = crate::paths::bookmarks_path();
        let _ = fs::write(path, serde_json::to_string_pretty(self).unwrap_or_default());
    }
}

#[derive(Clone, PartialEq)]
pub enum ViewMode {
    List,
    Gallery,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortKey {
    #[default]
    Name,
    Date,
    Size,
}

impl SortKey {
    pub fn label(self) -> &'static str {
        match self {
            Self::Name => "Name",
            Self::Date => "Date",
            Self::Size => "Size",
        }
    }

    /// Keep the familiar meaning when switching keys: names A→Z, but dates and
    /// sizes newest/largest first. The direction can still be toggled explicitly.
    pub fn default_direction(self) -> SortDirection {
        match self {
            Self::Name => SortDirection::Ascending,
            Self::Date | Self::Size => SortDirection::Descending,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortDirection {
    #[default]
    Ascending,
    Descending,
}

impl SortDirection {
    pub fn toggle(self) -> Self {
        match self {
            Self::Ascending => Self::Descending,
            Self::Descending => Self::Ascending,
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Ascending => "↑",
            Self::Descending => "↓",
        }
    }
}

/// A rating at or above this value is treated as "top rated" and floats to the
/// top of every folder, ahead of the normal type grouping and sort key.
pub const TOP_RATED_MIN: u8 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum FileKind {
    Image = 0,
    Video = 1,
    Audio = 2,
    Document = 3,
    Archive = 4,
    Other = 5,
}

fn normalized_ext(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .split('%')
        .next()
        .unwrap_or_default()
        .to_string()
}

pub fn file_kind(path: &Path, is_dir: bool) -> FileKind {
    if is_dir {
        return FileKind::Other;
    }
    let ext = normalized_ext(path);
    const IMAGE: &[&str] = &["jpg", "jpeg", "png", "gif", "webp", "bmp", "tif", "tiff", "heic", "heif", "avif", "ico", "svg"];
    const VIDEO: &[&str] = &["mp4", "m4v", "webm", "mkv", "avi", "mov", "wmv", "flv", "mpeg", "mpg"];
    const AUDIO: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "opus", "m4a"];
    const DOCUMENT: &[&str] = &["txt", "md", "pdf", "doc", "docx", "rtf", "odt", "csv", "json", "xml"];
    const ARCHIVE: &[&str] = &["zip", "rar", "7z", "tar", "gz", "bz2", "xz"];
    if IMAGE.contains(&ext.as_str()) {
        FileKind::Image
    } else if VIDEO.contains(&ext.as_str()) {
        FileKind::Video
    } else if AUDIO.contains(&ext.as_str()) {
        FileKind::Audio
    } else if DOCUMENT.contains(&ext.as_str()) {
        FileKind::Document
    } else if ARCHIVE.contains(&ext.as_str()) {
        FileKind::Archive
    } else {
        FileKind::Other
    }
}

/// The fields shared by list entries and gallery items for sorting.
#[derive(Clone, Debug)]
pub struct SortableEntry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub rating: u8,
    pub len: u64,
    pub date: i64,
}

fn sort_bucket(entry: &SortableEntry) -> u8 {
    if entry.is_dir {
        0
    } else if entry.rating >= TOP_RATED_MIN {
        1
    } else {
        2 + file_kind(&entry.path, false) as u8
    }
}

/// Smart ordering used by both the file list and the gallery.
///
/// Priority is: folders → top-rated files (4★+, by rating) → normal files grouped
/// by type → the selected key/direction. The chosen key never breaks the smart
/// grouping, so top-rated media stays at the top whether sorting by name or date.
pub fn compare_sortable(a: &SortableEntry, b: &SortableEntry, key: SortKey, direction: SortDirection) -> Ordering {
    let bucket = sort_bucket(a).cmp(&sort_bucket(b));
    if bucket != Ordering::Equal {
        return bucket;
    }

    if sort_bucket(a) == 1 && a.rating != b.rating {
        return b.rating.cmp(&a.rating);
    }

    let kind = file_kind(&a.path, a.is_dir).cmp(&file_kind(&b.path, b.is_dir));
    if kind != Ordering::Equal {
        return kind;
    }

    let key_order = match key {
        SortKey::Name => a.name.to_lowercase().cmp(&b.name.to_lowercase()),
        SortKey::Date => a.date.cmp(&b.date),
        SortKey::Size => a.len.cmp(&b.len),
    };
    let key_order = if direction == SortDirection::Descending {
        key_order.reverse()
    } else {
        key_order
    };

    key_order
        .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        .then_with(|| a.path.cmp(&b.path))
}

pub struct FileEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub rating: u8,
}

pub struct FileManagerState {
    pub current_dir: PathBuf,
    pub history: Vec<PathBuf>,
    pub history_idx: usize,
    pub entries: Vec<FileEntry>,
    pub selected: HashSet<PathBuf>,
    pub selection_anchor: Option<PathBuf>,
    pub view_mode: ViewMode,
    pub show_hidden: bool,
    pub search_query: String,
    pub mounts: Vec<PathBuf>,
    pub bookmarks: Bookmarks,
    pub sort: SortKey,
    pub sort_direction: SortDirection,
    pub list_scroll_offset: f32,
}

impl FileManagerState {
    pub fn new(initial_dir: PathBuf) -> Self {
        let mut s = Self {
            current_dir: initial_dir,
            history: Vec::new(),
            history_idx: 0,
            entries: Vec::new(),
            selected: HashSet::new(),
            selection_anchor: None,
            view_mode: ViewMode::List,
            show_hidden: false,
            search_query: String::new(),
            mounts: Vec::new(),
            bookmarks: Bookmarks::load(),
            sort: SortKey::default(),
            sort_direction: SortKey::default().default_direction(),
            list_scroll_offset: 0.0,
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
        if !path.exists() || !path.is_dir() {
            return;
        }
        self.current_dir = path.clone();
        self.selected.clear();
        self.selection_anchor = None;
        self.search_query.clear();
        self.list_scroll_offset = 0.0;

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

    pub fn set_sort(&mut self, sort: SortKey, direction: SortDirection) {
        self.sort = sort;
        self.sort_direction = direction;
        self.sort_entries();
    }

    pub fn toggle_sort_direction(&mut self) {
        self.sort_direction = self.sort_direction.toggle();
        self.sort_entries();
    }

    fn sort_entries(&mut self) {
        let meta: HashMap<PathBuf, (u64, i64)> = self
            .entries
            .iter()
            .map(|entry| (entry.path.clone(), metadata_key(&entry.path)))
            .collect();
        let sort = self.sort;
        let direction = self.sort_direction;
        self.entries.sort_by(|a, b| {
            let (a_len, a_date) = meta.get(&a.path).copied().unwrap_or((0, 0));
            let (b_len, b_date) = meta.get(&b.path).copied().unwrap_or((0, 0));
            compare_sortable(
                &SortableEntry {
                    path: a.path.clone(),
                    name: a.name.clone(),
                    is_dir: a.is_dir,
                    rating: a.rating,
                    len: a_len,
                    date: a_date,
                },
                &SortableEntry {
                    path: b.path.clone(),
                    name: b.name.clone(),
                    is_dir: b.is_dir,
                    rating: b.rating,
                    len: b_len,
                    date: b_date,
                },
                sort,
                direction,
            )
        });
    }

    pub fn refresh(&mut self) {
        self.entries.clear();
        if let Ok(rd) = fs::read_dir(&self.current_dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Dotfile sidecars stay hidden unless the user asks for them.
                if !self.show_hidden && name.starts_with('.') {
                    continue;
                }
                let path = entry.path();
                let is_dir = path.is_dir();
                let rating = if is_dir { 0 } else { crate::gallery::sidecar::Sidecar::quick_rating(&path) };
                self.entries.push(FileEntry {
                    name,
                    path,
                    is_dir,
                    rating,
                });
            }
        }
        self.sort_entries();
    }

    pub fn search_recursive(&mut self) {
        if self.search_query.is_empty() {
            self.refresh();
            return;
        }
        let query = self.search_query.to_lowercase();
        self.entries.clear();

        let mut stack = vec![self.current_dir.clone()];
        while let Some(dir) = stack.pop() {
            if let Ok(rd) = fs::read_dir(&dir) {
                for entry in rd.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !self.show_hidden && name.starts_with('.') {
                        continue;
                    }

                    if name.to_lowercase().contains(&query) {
                        let path = entry.path();
                        let is_dir = path.is_dir();
                        let rating = if is_dir { 0 } else { crate::gallery::sidecar::Sidecar::quick_rating(&path) };
                        self.entries.push(FileEntry {
                            name: name.clone(),
                            path,
                            is_dir,
                            rating,
                        });
                    }
                    if entry.path().is_dir() && !entry.path().is_symlink() {
                        stack.push(entry.path());
                    }
                }
            }
        }
        self.sort_entries();
    }

    pub fn open_terminal(&self) {
        for t in ["konsole", "gnome-terminal", "alacritty", "kitty", "xterm"] {
            if std::process::Command::new(t)
                .current_dir(&self.current_dir)
                .spawn()
                .is_ok()
            {
                return;
            }
        }
    }

    pub fn open_smart(&self, path: &Path) {
        if path.is_dir() {
            let _ = open::that(path);
            return;
        }
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if ["txt", "md", "rs", "toml", "json", "py", "js", "ts", "c", "cpp", "h", "sh", "cfg", "ini"]
            .contains(&ext.as_str())
        {
            if std::process::Command::new("kate").arg(path).spawn().is_ok() {
                return;
            }
        }
        let _ = open::that(path);
    }

    pub fn trash_selected(&mut self) {
        for p in &self.selected {
            let _ = trash::delete(p);
        }
        self.refresh();
    }

    pub fn add_bookmark(&mut self) {
        if !self.bookmarks.paths.contains(&self.current_dir) {
            self.bookmarks.paths.push(self.current_dir.clone());
            self.bookmarks.save();
        }
    }
}

fn metadata_key(path: &Path) -> (u64, i64) {
    fs::metadata(path)
        .map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (m.len(), modified)
        })
        .unwrap_or((0, 0))
}

fn valid_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name == "." || name == ".." {
        return Err("Enter a valid name".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err("A name cannot contain a path separator".to_string());
    }
    Ok(())
}

/// Add ` (copy)` / ` (copy 2)` before the extension until the path is free.
pub fn unique_path(original: &Path) -> PathBuf {
    let parent = original.parent().unwrap_or_else(|| Path::new("."));
    let name = original.file_name().unwrap_or_default().to_string_lossy();
    let (stem, ext) = match original.file_stem() {
        Some(stem) => (
            stem.to_string_lossy().into_owned(),
            original.extension().map(|e| e.to_string_lossy().into_owned()),
        ),
        None => (name.into_owned(), None),
    };

    for n in 0..1000 {
        let suffix = if n == 0 {
            " (copy)".to_string()
        } else {
            format!(" (copy {})", n + 1)
        };
        let candidate = match &ext {
            Some(ext) => format!("{stem}{suffix}.{ext}"),
            None => format!("{stem}{suffix}"),
        };
        let path = parent.join(candidate);
        if !path.exists() {
            return path;
        }
    }
    parent.join(format!("{stem} (copy {})", 1001))
}

fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
    if from.is_dir() {
        fs::create_dir_all(to)?;
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(from, to).map(|_| ())
    }
}

fn remove_tree(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

/// Remove files/directories created by an operation, for undo. Returns how many existed.
pub fn remove_items(paths: &[PathBuf]) -> Result<usize, String> {
    let mut done = 0;
    for path in paths {
        if fs::symlink_metadata(path).is_ok() {
            remove_tree(path).map_err(|e| format!("{}: {}", path.display(), e))?;
            done += 1;
        }
    }
    Ok(done)
}

fn move_tree(from: &Path, to: &Path) -> io::Result<()> {
    if to.starts_with(from) {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "cannot move an item into itself",
        ));
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Cross-device rename: copy first, only remove the source on success.
            if from.exists() && !to.exists() {
                copy_tree(from, to).and_then(|_| remove_tree(from))
            } else {
                Err(err)
            }
        }
    }
}

/// True when both paths resolve to the same existing item.
fn same_path(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

fn destination_for(source: &Path, dir: &Path, copy: bool) -> PathBuf {
    let name = source.file_name().unwrap_or_default();
    let candidate = dir.join(name);

    // A plain drag onto the item's own folder is a no-op, not a collision with
    // itself. Ctrl+drag still duplicates, as in a file manager.
    if !copy && same_path(&candidate, source) {
        return source.to_path_buf();
    }

    if candidate.exists() {
        unique_path(&candidate)
    } else {
        candidate
    }
}

/// Copy or move `paths` into `dir`. Returns the actual `(source, destination)` pairs so the
/// caller can make the operation undoable (unique destination names are chosen here).
pub fn transfer(paths: &[PathBuf], dir: &Path, copy: bool) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // A selection can contain a folder and something inside it. Handling the
    // folder already covers the child, so skip nested sources to avoid a
    // partial second copy/move after the parent has gone.
    let mut roots: Vec<&Path> = Vec::new();
    for from in paths {
        if roots.iter().any(|root| from.starts_with(root)) {
            continue;
        }
        roots.push(from.as_path());
    }

    let mut done = Vec::new();
    for from in roots {
        let to = destination_for(from, dir, copy);
        if !copy && same_path(&to, from) {
            continue;
        }
        // Copying a directory into its own subtree would recurse forever.
        let result = if to.starts_with(from) {
            Err(io::Error::new(
                ErrorKind::InvalidInput,
                "cannot copy an item into itself",
            ))
        } else if copy {
            copy_tree(from, &to)
        } else {
            move_tree(from, &to)
        };
        result.map_err(|e| format!("{}: {}", from.display(), e))?;
        done.push((from.to_path_buf(), to));
    }
    Ok(done)
}

pub fn duplicate(paths: &[PathBuf]) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    let mut roots: Vec<&Path> = Vec::new();
    for from in paths {
        if roots.iter().any(|root| from.starts_with(root)) {
            continue;
        }
        roots.push(from.as_path());
    }

    let mut done = Vec::new();
    for from in roots {
        let to = unique_path(from);
        copy_tree(from, &to).map_err(|e| format!("{}: {}", from.display(), e))?;
        done.push((from.to_path_buf(), to));
    }
    Ok(done)
}

pub fn rename(path: &Path, name: &str) -> Result<PathBuf, String> {
    valid_name(name)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let to = parent.join(name);
    if to != path && to.exists() {
        return Err(format!("'{}' already exists", name));
    }
    fs::rename(path, &to).map_err(|e| e.to_string())?;
    Ok(to)
}

pub fn create_item(dir: &Path, name: &str, is_dir: bool) -> Result<PathBuf, String> {
    valid_name(name)?;
    let path = dir.join(name);
    if path.exists() {
        return Err(format!("'{}' already exists", name));
    }
    if is_dir {
        fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    } else {
        File::create(&path).map_err(|e| e.to_string())?;
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "symtropy-fm-{}-{}-{}",
            std::process::id(),
            name,
            unique
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn rejects_names_with_separators() {
        assert!(valid_name("ok.txt").is_ok());
        assert!(valid_name("../escape").is_err());
        assert!(valid_name("a/b").is_err());
        assert!(valid_name("").is_err());
    }

    #[test]
    fn unique_path_appends_copy() {
        let root = scratch("unique");
        let p = root.join("photo.webp");
        assert_eq!(
            unique_path(&p),
            root.join("photo (copy).webp")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transfer_copies_and_moves_trees() {
        let root = scratch("transfer");
        let src = root.join("src");
        let nested = src.join("nested");
        let copied_to = root.join("copied");
        let moved_to = root.join("moved");
        fs::create_dir_all(&nested).unwrap();
        fs::create_dir_all(&copied_to).unwrap();
        fs::create_dir_all(&moved_to).unwrap();
        fs::write(src.join("a.txt"), "a").unwrap();
        fs::write(nested.join("b.txt"), "b").unwrap();

        assert_eq!(transfer(&[src.clone()], &copied_to, true).unwrap().len(), 1);
        assert!(src.join("a.txt").exists());
        assert_eq!(
            fs::read_to_string(copied_to.join("src/nested/b.txt")).unwrap(),
            "b"
        );

        assert_eq!(transfer(&[src.clone()], &moved_to, false).unwrap().len(), 1);
        assert!(!src.exists());
        assert_eq!(fs::read_to_string(moved_to.join("src/a.txt")).unwrap(), "a");

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transfer_rejects_copy_into_own_subtree() {
        let root = scratch("self-copy");
        let src = root.join("src");
        let inner = src.join("inner");
        fs::create_dir_all(&inner).unwrap();
        assert!(transfer(&[src.clone()], &inner, true).is_err());
        assert!(!src.join("inner/src").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn move_onto_own_folder_is_noop_but_copy_duplicates() {
        let root = scratch("self-drop");
        let file = root.join("photo.webp");
        fs::write(&file, "image").unwrap();

        assert!(transfer(&[file.clone()], &root, false).unwrap().is_empty());
        assert!(file.exists());
        assert!(!root.join("photo (copy).webp").exists());

        let pairs = transfer(&[file.clone()], &root, true).unwrap();
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].0, file);
        assert_eq!(pairs[0].1, root.join("photo (copy).webp"));
        assert!(file.exists());
        assert!(root.join("photo (copy).webp").exists());
        assert_eq!(remove_items(&[pairs[0].1.clone()]).unwrap(), 1);
        assert!(!root.join("photo (copy).webp").exists());

        fs::remove_dir_all(root).unwrap();
    }

    fn sortable(name: &str, is_dir: bool, rating: u8) -> SortableEntry {
        SortableEntry {
            path: PathBuf::from(format!("/tmp/{name}")),
            name: name.to_string(),
            is_dir,
            rating,
            len: 0,
            date: 0,
        }
    }

    fn names(entries: &[SortableEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    #[test]
    fn smart_sort_keeps_folders_top_rated_and_type_groups_ahead_of_key() {
        let mut entries = vec![
            sortable("z.txt", false, 5),
            sortable("folder", true, 0),
            sortable("a.mp4", false, 0),
            sortable("b.png", false, 0),
            sortable("c.txt", false, 0),
        ];
        entries.sort_by(|a, b| compare_sortable(a, b, SortKey::Name, SortDirection::Descending));
        assert_eq!(
            names(&entries),
            vec!["folder", "z.txt", "b.png", "a.mp4", "c.txt"]
        );
    }

    #[test]
    fn sort_direction_reverses_only_within_type_group() {
        let mut entries = vec![
            sortable("a.png", false, 0),
            sortable("m.mp4", false, 0),
            sortable("folder", true, 0),
            sortable("z.png", false, 0),
        ];
        entries.sort_by(|a, b| compare_sortable(a, b, SortKey::Name, SortDirection::Ascending));
        assert_eq!(names(&entries), vec!["folder", "a.png", "z.png", "m.mp4"]);

        entries.sort_by(|a, b| compare_sortable(a, b, SortKey::Name, SortDirection::Descending));
        assert_eq!(names(&entries), vec!["folder", "z.png", "a.png", "m.mp4"]);
    }
}
