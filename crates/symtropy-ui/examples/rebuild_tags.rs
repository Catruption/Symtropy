//! Rebuilds the tags.txt hierarchy from `.halo.json` sidecars.
//! Usage: cargo run --release -p symtropy-ui --example rebuild_tags database/References

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use rayon::prelude::*;

#[derive(serde::Deserialize)]
struct MiniSidecar {
    #[serde(default)] tag_parents: HashMap<String, String>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = if args.len() > 1 { Path::new(&args[1]) } else { Path::new("database/References") };
    
    println!("Scanning {} for .halo.json sidecars...", dir.display());
    let entries: Vec<_> = fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();

    let merged_parents = entries.par_iter().fold(
        || HashMap::new(),
        |mut acc, path| {
            if path.extension().and_then(|e| e.to_str()) == Some("json") && path.to_string_lossy().ends_with(".halo.json") {
                if let Ok(text) = fs::read_to_string(path) {
                    if let Ok(sc) = serde_json::from_str::<MiniSidecar>(&text) {
                        for (child, parent) in sc.tag_parents {
                            if !parent.is_empty() {
                                acc.insert(child, parent);
                            }
                        }
                    }
                }
            }
            acc
        }
    ).reduce(|| HashMap::new(), |mut a, b| {
        a.extend(b);
        a
    });

    let mut children_map: HashMap<String, Vec<String>> = HashMap::new();
    let mut all_children = HashSet::new();

    for (child, parent) in &merged_parents {
        children_map.entry(parent.clone()).or_default().push(child.clone());
        all_children.insert(child.clone());
    }

    let mut roots: Vec<String> = children_map.keys()
        .filter(|k| !all_children.contains(*k))
        .cloned()
        .collect();
    roots.sort();

    fn print_tree(node: &str, depth: usize, children_map: &HashMap<String, Vec<String>>, out: &mut String) {
        let indent = "  ".repeat(depth);
        out.push_str(&format!("{}{}\n", indent, node));
        if let Some(children) = children_map.get(node) {
            let mut sorted = children.clone();
            sorted.sort();
            for child in sorted {
                print_tree(&child, depth + 1, children_map, out);
            }
        }
    }

    let mut out = String::new();
    for root in roots {
        print_tree(&root, 0, &children_map, &mut out);
    }

    let out_path = dir.join("../tags.txt");
    if fs::write(&out_path, out).is_ok() {
        println!("Rebuilt tag hierarchy successfully at {}", out_path.display());
    }
}