use ssvg::Package;
use std::{env, fs, process};

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let as_json = args.iter().any(|a| a == "--json");
    let paths: Vec<&String> = args.iter().filter(|a| a.as_str() != "--json").collect();
    let path = match paths.as_slice() {
        [path] => path,
        _ => {
            eprintln!("usage: inspect <file.ssvg> [--json]");
            process::exit(2);
        }
    };

    let bytes = fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        process::exit(1);
    });
    let package = Package::read_from(std::io::Cursor::new(&bytes)).unwrap_or_else(|e| {
        eprintln!("cannot open {path} as ssvg: {e}");
        process::exit(1);
    });

    let (manifest, scene) = if as_json {
        package.to_json_bundle().unwrap_or_else(|e| {
            eprintln!("cannot export JSON: {e}");
            process::exit(1);
        })
    } else {
        package.to_json5_bundle().unwrap_or_else(|e| {
            eprintln!("cannot export JSON5: {e}");
            process::exit(1);
        })
    };

    println!("{manifest}");
    println!("{scene}");
    println!(
        "assets={} extras={} history={}",
        package.assets.len(),
        package.extras.len(),
        package.extras.keys().filter(|p| p.starts_with("history/")).count()
    );
}
