//! Convert a Krita project directly into `.ssvg` without a WebP export step.
//!
//! Usage:
//!   cargo run -p ssvg --example kra-direct-to-ssvg -- <project.kra> <out.ssvg>

use ssvg::convert_kra_direct;
use std::{env, fs, path::PathBuf, process};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() != 2 {
        eprintln!("usage: kra-direct-to-ssvg <project.kra> <out.ssvg>");
        process::exit(2);
    }
    let kra = PathBuf::from(&args[0]);
    let output = PathBuf::from(&args[1]);

    if let Some(parent) = output.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("cannot create {}: {e}", parent.display());
                process::exit(1);
            });
        }
    }

    let report = convert_kra_direct(&kra, &output).unwrap_or_else(|e| {
        eprintln!("conversion failed: {e}");
        process::exit(1);
    });

    println!("wrote {}", output.display());
    println!(
        "layers={} groups={} decoded_krita={}",
        report.layers, report.groups, report.matched_krita_layers
    );
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
}
