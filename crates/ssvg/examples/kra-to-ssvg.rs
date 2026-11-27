//! Convert a Krita project plus its exported WebP layer tree into `.ssvg`.
//!
//! Usage:
//!   cargo run -p ssvg --example kra-to-ssvg -- \
//!       <project.kra> <exported-webp-dir> <out.ssvg>

use ssvg::{convert_kra, ConvertOptions};
use std::{env, fs, path::PathBuf, process};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!(
            "usage: kra-to-ssvg <project.kra> <exported-webp-dir> <out.ssvg>"
        );
        process::exit(2);
    }
    let kra = PathBuf::from(&args[0]);
    let exports = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("cannot create {}: {e}", parent.display());
                process::exit(1);
            });
        }
    }

    let report = convert_kra(&kra, &exports, &output).unwrap_or_else(|e| {
        eprintln!("conversion failed: {e}");
        process::exit(1);
    });

    println!("wrote {}", output.display());
    println!(
        "layers={} groups={} matched_krita={}",
        report.layers, report.groups, report.matched_krita_layers
    );
    for warning in &report.warnings {
        println!("warning: {warning}");
    }
    let _ = ConvertOptions::default();
}
