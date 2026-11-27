#![forbid(unsafe_code)]
//! CLI front end for the Symtropy agent bridge.
//!
//! `request` reads one JSON5 `AgentRequest` from stdin and writes one JSON
//! response to stdout. `artists --root <dir>` is a convenience batch job for
//! sidecar artist metadata.

use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process;
use symtropy_agent::{
    default_store_path, error_json, AgentBridge, AgentPolicy, AgentRequest, AgentResponse,
    ArtistSidecarInput, PreservationSidecarInput,
};
use symtropy_tags::Proposal;

const IMAGE_EXTS: &[&str] = &[
    "webp", "png", "jpg", "jpeg", "gif", "bmp", "avif", "tif", "tiff", "ssvg",
];

#[derive(Debug, Default)]
struct Options {
    store: Option<PathBuf>,
    root: Option<PathBuf>,
    apply: bool,
    include_accepted: bool,
    limit: usize,
    max_depth: usize,
}

fn usage() -> &'static str {
    r#"symtropy-agent — proposal-only bridge to Symtropy's tag system

Usage:
  symtropy-agent status [--store PATH]
  symtropy-agent request [--store PATH]          # JSON5 AgentRequest on stdin
  symtropy-agent preview [--store PATH]          # Proposal JSON5 on stdin
  symtropy-agent apply [--store PATH]            # Proposal JSON5 on stdin
  symtropy-agent artists --root DIR [--apply] [--limit N] [--max-depth N] [--store PATH]
  symtropy-agent preservation --root DIR [--include-accepted] [--limit N] [--max-depth N] [--store PATH]

Safety:
  - Agents cannot directly write the store; every mutation is a Proposal.
  - Only conservative/review policies are exposed.
  - External proposals claiming human provenance are downgraded to untrusted AI.
  - AI suggestions always stay below full confidence and need review.
"#
}

fn parse_args(args: &[String]) -> Result<(String, Options), String> {
    let Some(command) = args.first() else {
        return Ok(("help".to_string(), Options::default()));
    };
    let mut options = Options {
        max_depth: 32,
        ..Options::default()
    };
    let mut index = 1usize;
    while index < args.len() {
        let flag = args[index].as_str();
        match flag {
            "--store" | "-s" => {
                index += 1;
                options.store = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| format!("{} needs a path", flag))?,
                ));
            }
            "--root" | "-r" => {
                index += 1;
                options.root = Some(PathBuf::from(
                    args.get(index)
                        .ok_or_else(|| format!("{} needs a path", flag))?,
                ));
            }
            "--apply" => options.apply = true,
            "--preview" => options.apply = false,
            "--include-accepted" => options.include_accepted = true,
            "--limit" => {
                index += 1;
                options.limit = args
                    .get(index)
                    .ok_or_else(|| "--limit needs a number".to_string())?
                    .parse()
                    .map_err(|_| "--limit must be a number".to_string())?;
            }
            "--max-depth" => {
                index += 1;
                options.max_depth = args
                    .get(index)
                    .ok_or_else(|| "--max-depth needs a number".to_string())?
                    .parse()
                    .map_err(|_| "--max-depth must be a number".to_string())?;
            }
            "--help" | "-h" => return Ok(("help".to_string(), options)),
            other => return Err(format!("unknown option: {}", other)),
        }
        index += 1;
    }
    Ok((command.clone(), options))
}

fn bridge_for(options: &Options) -> Result<AgentBridge, String> {
    let path = options.store.clone().unwrap_or_else(default_store_path);
    AgentBridge::load(path)
}

fn read_stdin() -> Result<String, String> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| format!("could not read stdin: {}", error))?;
    Ok(input)
}

fn emit<T: serde::Serialize>(value: &T) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("could not serialize response: {}", error))?;
    println!("{}", text);
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct SidecarInput {
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    preserved_tags: Vec<String>,
    #[serde(default)]
    artists: Vec<String>,
    #[serde(default)]
    tag_parents: HashMap<String, String>,
    #[serde(default)]
    version: u32,
}

fn artist_hints_from_sidecar(sidecar: &SidecarInput) -> Vec<String> {
    sidecar
        .tag_parents
        .iter()
        .filter(|(_, parent)| {
            matches!(
                parent.trim().to_ascii_lowercase().as_str(),
                "artist" | "creator"
            )
        })
        .map(|(child, _)| child.clone())
        .collect()
}

fn is_halo_sidecar(name: &str) -> bool {
    name.ends_with(".halo.json")
}

fn collect_sidecars(
    root: &Path,
    max_depth: usize,
    limit: usize,
) -> Result<Vec<PathBuf>, String> {
    fn walk(
        dir: &Path,
        depth: usize,
        max_depth: usize,
        out: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        if depth > max_depth {
            return Ok(());
        }
        let entries = fs::read_dir(dir)
            .map_err(|error| format!("could not read {}: {}", dir.display(), error))?;
        let mut paths = Vec::new();
        for entry in entries {
            let entry = entry
                .map_err(|error| format!("could not read entry in {}: {}", dir.display(), error))?;
            paths.push(entry.path());
        }
        paths.sort();
        for path in paths {
            let metadata = fs::symlink_metadata(&path)
                .map_err(|error| format!("could not inspect {}: {}", path.display(), error))?;
            if metadata.is_dir() {
                walk(&path, depth + 1, max_depth, out)?;
            } else if metadata.is_file() {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if is_halo_sidecar(&name) {
                    out.push(path);
                }
            }
        }
        Ok(())
    }

    let mut found = Vec::new();
    walk(root, 0, max_depth, &mut found)?;
    found.sort();
    found.dedup();
    if limit > 0 && found.len() > limit {
        found.truncate(limit);
    }
    Ok(found)
}

fn sidecar_stem(path: &Path) -> Option<String> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let unhidden = name.strip_prefix('.').unwrap_or(name.as_str());
    unhidden
        .strip_suffix(".halo.json")
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
}

/// Index image paths by `(parent, stem)` in one pass. Without this, resolving
/// thousands of sidecars rescans each parent folder once per sidecar.
fn build_image_lookup(sidecars: &[PathBuf]) -> HashMap<(PathBuf, String), PathBuf> {
    let parents: HashSet<PathBuf> = sidecars
        .iter()
        .filter_map(|path| sidecar_stem(path).map(|_| path.parent().unwrap_or(Path::new(".")).to_path_buf()))
        .collect();

    let mut lookup = HashMap::new();
    for parent in parents {
        let mut images = Vec::new();
        if let Ok(entries) = fs::read_dir(&parent) {
            for entry in entries.flatten() {
                let candidate = entry.path();
                let Ok(metadata) = fs::symlink_metadata(&candidate) else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                let ext = candidate
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if !IMAGE_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                if let Some(stem) = candidate.file_stem().and_then(|value| value.to_str()) {
                    images.push((stem.to_string(), candidate));
                }
            }
        }
        images.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.cmp(&right.1)));
        for (stem, path) in images {
            lookup.entry((parent.clone(), stem)).or_insert(path);
        }
    }
    lookup
}

fn item_id_from_sidecar(path: &Path, lookup: &HashMap<(PathBuf, String), PathBuf>) -> String {
    let parent = path.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let Some(stem) = sidecar_stem(path) else {
        return path.to_string_lossy().into_owned();
    };
    lookup
        .get(&(parent.clone(), stem.clone()))
        .cloned()
        .unwrap_or_else(|| parent.join(stem))
        .to_string_lossy()
        .into_owned()
}

fn read_sidecars(root: &Path, options: &Options) -> Result<Vec<(PathBuf, SidecarInput)>, String> {
    let paths = collect_sidecars(root, options.max_depth, options.limit)?;
    let mut sidecars = Vec::new();
    for path in paths {
        let text = fs::read_to_string(&path)
            .map_err(|error| format!("could not read {}: {}", path.display(), error))?;
        let sidecar: SidecarInput = match serde_json::from_str(&text) {
            Ok(sidecar) => sidecar,
            Err(error) => {
                eprintln!("symtropy-agent: skipping {}: {}", path.display(), error);
                continue;
            }
        };
        sidecars.push((path, sidecar));
    }
    Ok(sidecars)
}

fn run_artists(root: &Path, options: &Options) -> Result<AgentResponse, String> {
    let mut bridge = bridge_for(options)?;
    let sidecars = read_sidecars(root, options)?;
    let paths: Vec<PathBuf> = sidecars.iter().map(|(path, _)| path.clone()).collect();
    let image_lookup = build_image_lookup(&paths);
    let mut items = Vec::new();
    for (path, sidecar) in sidecars {
        if sidecar.tags.is_empty() && sidecar.artists.is_empty() {
            continue;
        }
        let artist_hints = artist_hints_from_sidecar(&sidecar);
        items.push(ArtistSidecarInput {
            item_id: item_id_from_sidecar(&path, &image_lookup),
            tags: sidecar.tags,
            artists: sidecar.artists,
            artist_hints,
        });
    }

    eprintln!(
        "symtropy-agent: scanned {} sidecars with tag or artist metadata",
        items.len()
    );

    if options.apply {
        bridge.handle(AgentRequest::StageArtistSidecars { items })
    } else {
        bridge.handle(AgentRequest::ProposeArtistSidecars { items })
    }
}

fn run_preservation(root: &Path, options: &Options) -> Result<AgentResponse, String> {
    let mut bridge = bridge_for(options)?;
    let sidecars = read_sidecars(root, options)?;
    let paths: Vec<PathBuf> = sidecars.iter().map(|(path, _)| path.clone()).collect();
    let image_lookup = build_image_lookup(&paths);
    let mut items = Vec::new();
    for (path, sidecar) in sidecars {
        let artist_hints = artist_hints_from_sidecar(&sidecar);
        if sidecar.tags.is_empty() && sidecar.artists.is_empty() && artist_hints.is_empty() {
            continue;
        }
        items.push(PreservationSidecarInput {
            item_id: item_id_from_sidecar(&path, &image_lookup),
            tags: sidecar.tags,
            preserved_tags: sidecar.preserved_tags,
            artists: sidecar.artists,
            artist_hints,
            version: sidecar.version,
        });
    }

    eprintln!(
        "symtropy-agent: preserving {} sidecars with metadata",
        items.len()
    );
    bridge.handle(AgentRequest::BuildPreservationReport {
        items,
        include_accepted: options.include_accepted,
    })
}

fn run(args: &[String]) -> Result<(), String> {
    let (command, options) = parse_args(args)?;
    if command == "help" {
        println!("{}", usage());
        return Ok(());
    }

    match command.as_str() {
        "status" => {
            let mut bridge = bridge_for(&options)?;
            emit(&bridge.handle(AgentRequest::Status)?)
        }
        "request" => {
            let mut bridge = bridge_for(&options)?;
            let input = read_stdin()?;
            let response = bridge.handle_json(&input)?;
            println!("{}", response);
            Ok(())
        }
        "preview" => {
            let mut bridge = bridge_for(&options)?;
            let input = read_stdin()?;
            let proposal: Proposal = json5::from_str(&input)
                .map_err(|error| format!("invalid proposal JSON5: {}", error))?;
            let response = bridge.handle(AgentRequest::PreviewProposal {
                proposal,
                policy: AgentPolicy::Conservative,
            })?;
            emit(&response)
        }
        "apply" => {
            let mut bridge = bridge_for(&options)?;
            let input = read_stdin()?;
            let proposal: Proposal = json5::from_str(&input)
                .map_err(|error| format!("invalid proposal JSON5: {}", error))?;
            let response = bridge.handle(AgentRequest::ApplyProposal {
                proposal,
                policy: AgentPolicy::Conservative,
            })?;
            emit(&response)
        }
        "artists" => {
            let root = options
                .root
                .clone()
                .ok_or_else(|| "artists needs --root DIR".to_string())?;
            if !root.is_dir() {
                return Err(format!("{} is not a directory", root.display()));
            }
            emit(&run_artists(&root, &options)?)
        }
        "preservation" => {
            let root = options
                .root
                .clone()
                .ok_or_else(|| "preservation needs --root DIR".to_string())?;
            if !root.is_dir() {
                return Err(format!("{} is not a directory", root.display()));
            }
            emit(&run_preservation(&root, &options)?)
        }
        other => Err(format!("unknown command: {}", other)),
    }
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if let Err(error) = run(&args) {
        eprintln!("{}", error_json(error));
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use symtropy_tags::{ApplyPolicy, TagStore};

    #[test]
    fn halo_sidecar_names_are_detected() {
        assert!(is_halo_sidecar(".fox.halo.json"));
        assert!(is_halo_sidecar("fox.halo.json"));
        assert!(!is_halo_sidecar(".fox.halo.json.tmp"));
        assert!(!is_halo_sidecar("fox.webp"));
    }

    #[test]
    fn artist_hints_come_from_tag_parents() {
        let mut parents = HashMap::new();
        parents.insert("miranii".to_string(), "artist".to_string());
        parents.insert("feline".to_string(), "animal".to_string());
        parents.insert("zedkat".to_string(), "Creator".to_string());
        let sidecar = SidecarInput {
            tags: vec![],
            preserved_tags: vec![],
            artists: vec![],
            tag_parents: parents,
            version: 1,
        };
        let mut hints = artist_hints_from_sidecar(&sidecar);
        hints.sort();
        assert_eq!(hints, vec!["miranii".to_string(), "zedkat".to_string()]);
    }

    #[test]
    fn image_lookup_resolves_sidecar_to_its_image() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("symtropy-agent-lookup-{}", unique));
        fs::create_dir_all(&dir).unwrap();
        let image = dir.join("fox.webp");
        let sidecar = dir.join(".fox.halo.json");
        fs::write(&image, b"").unwrap();
        fs::write(&sidecar, b"{}").unwrap();

        let lookup = build_image_lookup(&[sidecar.clone()]);
        assert_eq!(item_id_from_sidecar(&sidecar, &lookup), image.to_string_lossy());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn batch_artist_proposals_keep_import_provenance() {
        let items = vec![ArtistSidecarInput {
            item_id: "/library/fox.webp".to_string(),
            tags: vec!["fox".to_string(), "kitty".to_string()],
            artists: vec!["Kitty".to_string()],
            artist_hints: vec![],
        }];
        let proposal = symtropy_agent::propose_artist_sidecars(&items, 0);
        assert_eq!(proposal.provenance.kind, symtropy_tags::ProvenanceKind::Import);

        let store = TagStore::default();
        let preview = store.preview(&proposal, &ApplyPolicy::conservative());
        assert!(preview.rejected.is_empty());
        assert_eq!(preview.planned.len(), 3);
    }
}
