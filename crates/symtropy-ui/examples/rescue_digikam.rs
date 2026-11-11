//! Rescue tags directly from DigiKam's SQLite database.
//! Usage: cargo run --release -p symtropy-ui --example rescue_digikam <path_to_digikam4.db> <fallback_dir>

use rusqlite::{Connection, OpenFlags};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Sidecar {
    pub version: u32,
    pub tags: Vec<String>,
    pub tag_parents: HashMap<String, String>,
    pub rating: u8,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <path_to_digikam4.db> <fallback_dir>", args[0]);
        return;
    }

    let db_path = Path::new(&args[1]);
    let fallback_dir = Path::new(&args[2]);

    println!("Opening DigiKam DB at {} (Read-Only)...", db_path.display());

    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ).expect("Failed to open database.");

    // 1. Read the tag hierarchy
    println!("Reading tag hierarchy...");
    let mut stmt = conn.prepare("SELECT id, pid, name FROM Tags").unwrap();
    let tag_iter = stmt.query_map([], |row| {
        Ok((row.get::<_, i32>(0)?, row.get::<_, i32>(1)?, row.get::<_, String>(2)?))
    }).unwrap();

    let mut tags_by_id = HashMap::new();
    let mut parent_map: HashMap<i32, String> = HashMap::new();

    for tag in tag_iter.flatten() {
        tags_by_id.insert(tag.0, tag.2.clone());
        if tag.1 != 0 {
            if let Some(parent_name) = tags_by_id.get(&tag.1) {
                parent_map.insert(tag.0, parent_name.clone());
            }
        }
    }

    // 2. Read image parts and tags
    println!("Reading image paths and tags...");
    let mut stmt = conn.prepare("
    SELECT
    AlbumRoots.specificPath,
    Albums.relativePath,
    Images.name,
    Tags.id,
    Tags.name
    FROM ImageTags
    JOIN Images ON ImageTags.imageid = Images.id
    JOIN Tags ON ImageTags.tagid = Tags.id
    JOIN Albums ON Images.album = Albums.id
    JOIN AlbumRoots ON Albums.albumRoot = AlbumRoots.id
    ").unwrap();

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, i32>(3)?,
            row.get::<_, String>(4)?,
        ))
    }).unwrap();

    let mut image_tags: HashMap<PathBuf, Vec<(i32, String)>> = HashMap::new();
    let mut printed_missing = 0;

    for row in rows.flatten() {
        let (root, rel, name, tag_id, tag_name) = row;

        let root = urlencoding::decode(&root).map(|s| s.into_owned()).unwrap_or(root);
        let rel = urlencoding::decode(&rel).map(|s| s.into_owned()).unwrap_or(rel);
        let name = urlencoding::decode(&name).map(|s| s.into_owned()).unwrap_or(name);

        let root_clean = root.replace("file://", "");
        let mut path = PathBuf::from(root_clean);
        path.push(rel.trim_start_matches('/'));
        path.push(name);

        // Try to find the file. If it doesn't exist, try the fallback heuristics.
        let mut final_path = path.clone();

        // Heuristic 1: Prepend mount root if it starts with /Pictures
        if !final_path.exists() && final_path.starts_with("/Pictures") {
            let rel_path = final_path.strip_prefix("/").unwrap();
            let mounted = PathBuf::from("/run/media/cashmere/Creationism").join(rel_path);
            if mounted.exists() {
                final_path = mounted;
            }
        }

        // Heuristic 2: Try the fallback dir with original extension
        if !final_path.exists() {
            let f = fallback_dir.join(path.file_name().unwrap_or_default());
            if f.exists() { final_path = f; }
        }

        // Heuristic 3: Try the fallback dir with .webp extension
        if !final_path.exists() {
            let mut webp_name = path.file_stem().unwrap_or_default().to_os_string();
            webp_name.push(".webp");
            let f = fallback_dir.join(webp_name);
            if f.exists() { final_path = f; }
        }

        // Final check
        if !final_path.exists() {
            if printed_missing < 5 {
                println!("[MISSING] Could not find: {}", path.display());
                printed_missing += 1;
            }
            continue;
        }

        image_tags.entry(final_path).or_default().push((tag_id, tag_name));
    }

    println!("Found {} images that actually exist on disk.", image_tags.len());

    // 3. Write the sidecars
    println!("Writing .halo.json files...");
    let mut rescued = 0;

    for (img_path, tags) in image_tags {
        let json_path = img_path.with_extension("halo.json");
        let mut sc = Sidecar { version: 1, tags: Vec::new(), tag_parents: HashMap::new(), rating: 0 };

        for (tag_id, tag_name) in tags {
            if !sc.tags.contains(&tag_name) { sc.tags.push(tag_name.clone()); }

            let mut current_id = tag_id;
            while let Some(parent_name) = parent_map.get(&current_id) {
                let current_name = tags_by_id.get(&current_id).unwrap().clone();
                if !sc.tags.contains(parent_name) { sc.tags.push(parent_name.clone()); }
                sc.tag_parents.insert(current_name, parent_name.clone());

                let parent_id = tags_by_id.iter().find(|(_, v)| **v == *parent_name).map(|(k, _)| *k);
                if let Some(pid) = parent_id { current_id = pid; } else { break; }
            }
        }

        let text = serde_json::to_string_pretty(&sc).unwrap();
        if fs::write(&json_path, text).is_ok() { rescued += 1; }
    }
    println!("Rescued {} images from DigiKam DB!", rescued);
}
