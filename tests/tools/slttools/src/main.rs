//! slttools: yuzhu 共有 slt スイートの補助ツール（11-tests-plan.md §3.2.8）。
//!
//!   slttools lint [--only L01,L02] [--known-diffs FILE] [paths...]
//!   slttools consistency [--add] [--check] [paths...]
//!
//!   slttools plan-variants base|expand|check
//!
//! 未実装:  large-keys / pgregress（K3・K1 との共同の部分）。

mod consistency;
mod lint;
mod parse;
mod planvar;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

fn repo_root() -> PathBuf {
    let mut d = std::env::current_dir().unwrap_or_default();
    loop {
        if d.join("tests/run.sh").exists() {
            return d;
        }
        if !d.pop() {
            return PathBuf::from(".");
        }
    }
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:\n  slttools lint [--only L01,L02] [--known-diffs FILE] [--warnings-as-errors] [paths...]\n  slttools consistency [--add] [--check] [paths...]\n  slttools plan-variants base|expand|check"
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(cmd) = args.next() else {
        return usage();
    };
    let rest: Vec<String> = args.collect();
    let root = repo_root();
    match cmd.as_str() {
        "lint" => {
            let mut only = Vec::new();
            let mut kd = Some(root.join("tests/slt/m4/KNOWN-DIFFS.md"));
            let mut paths: Vec<PathBuf> = Vec::new();
            let mut werror = false;
            let mut it = rest.iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--only" => only.extend(
                        it.next()
                            .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
                            .unwrap_or_default(),
                    ),
                    "--known-diffs" => kd = it.next().map(PathBuf::from),
                    "--warnings-as-errors" => werror = true,
                    "-h" | "--help" => return usage(),
                    s if s.starts_with('-') => return usage(),
                    s => paths.push(PathBuf::from(s)),
                }
            }
            let whole = paths.is_empty();
            if whole {
                paths = vec![root.join("tests/slt"), root.join("tests/restart")];
            }
            let o = lint::Options {
                only,
                known_diffs: kd.filter(|p| Path::new(p).exists()),
                whole,
            };
            let diags = lint::run(&paths, &o);
            let (mut errs, mut warns) = (0, 0);
            for d in &diags {
                println!("{}", lint::format_diag(d));
                if d.warning {
                    warns += 1;
                } else {
                    errs += 1;
                }
            }
            eprintln!("slttools lint: {errs} error(s), {warns} warning(s)");
            if errs > 0 || (werror && warns > 0) {
                ExitCode::FAILURE
            } else {
                ExitCode::SUCCESS
            }
        }
        "consistency" => {
            let add = rest.iter().any(|a| a == "--add");
            let check = rest.iter().any(|a| a == "--check");
            let paths: Vec<PathBuf> = rest
                .iter()
                .filter(|a| !a.starts_with('-'))
                .map(PathBuf::from)
                .collect();
            match consistency::run(&root, &paths, add, check) {
                Ok(changed) => {
                    for c in &changed {
                        println!(
                            "{} {}",
                            if check { "differs:" } else { "updated:" },
                            c.display()
                        );
                    }
                    if check && !changed.is_empty() {
                        ExitCode::FAILURE
                    } else {
                        ExitCode::SUCCESS
                    }
                }
                Err(e) => {
                    eprintln!("slttools consistency: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        "plan-variants" => match planvar::run(&root, &rest) {
            Ok(bad) => {
                for b in &bad {
                    println!("differs: {}", b.display());
                }
                if bad.is_empty() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }
            }
            Err(e) => {
                eprintln!("slttools plan-variants: {e}");
                ExitCode::FAILURE
            }
        },
        _ => usage(),
    }
}
