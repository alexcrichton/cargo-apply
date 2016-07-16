extern crate cargo;
extern crate git2;
extern crate walkdir;
extern crate libc;

use std::env;
use std::fs::{self, File};
use std::io::prelude::*;
use std::io;
use std::path::{Path, PathBuf};

use cargo::core::{Source, SourceId, Registry, Dependency};
use cargo::ops;
use cargo::sources::RegistrySource;
use cargo::core::shell::{Shell, MultiShell, Verbosity, ShellConfig, ColorConfig};
use cargo::util::Config;
use walkdir::{WalkDir, DirEntry, WalkDirIterator};

fn main() {
    let ref work_dir = PathBuf::from("work");
    let ref index_path = work_dir.join("index");
    let ref tmp_index_path = work_dir.join(".index");
    let ref cargo_dir = work_dir.join(".cargo");
    let ref stdio_dir = work_dir.join("stdio");
    let ref results_dir = work_dir.join("results");

    fs::create_dir_all(cargo_dir).unwrap();
    fs::create_dir_all(stdio_dir).unwrap();
    fs::create_dir_all(results_dir).unwrap();

    if fs::metadata(index_path).is_err() {
        println!("initializing registry index");
        git2::Repository::clone("https://github.com/rust-lang/crates.io-index",
                                tmp_index_path).unwrap();
        fs::rename(tmp_index_path, index_path).unwrap();
    }

    // Update the Cargo registry just once before we begin
    let config = config(&work_dir, Box::new(io::stdout()), Box::new(io::stderr()));
    let id = SourceId::for_central(&config).unwrap();
    let mut s = RegistrySource::new(&id, &config);
    s.update().unwrap();

    let cargo_config = format!("
        [build]
        target-dir = './target'
    ");

    File::create(cargo_dir.join("config")).unwrap()
        .write_all(cargo_config.as_bytes()).unwrap();

    let crates = WalkDir::new(index_path).into_iter()
        .filter_entry(|e| !bad(e))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.file_name().to_str().unwrap().to_string());

    for krate in crates {
        let ref crate_stdio_dir = stdio_dir.join(&krate);
        let ref crate_result_dir = results_dir.join(&krate);
        let ref result_file = crate_result_dir.join("results.txt");

        fs::create_dir_all(crate_stdio_dir).unwrap();
        fs::create_dir_all(crate_result_dir).unwrap();

        if fs::metadata(result_file).is_ok() {
            println!("using existing results for: {}", krate);
            continue;
        }

        println!("working on: {}", krate);

        let result = build(work_dir, crate_stdio_dir, &krate);
        report_result(&result_file, result);
    }
}

fn report_result(result_file: &Path, r: BuildResult) {
    let s = match r {
        BuildResult::Success => "ok".to_string(),
        BuildResult::TestFail(e) => format!("bad test: {}", e),
        BuildResult::BuildFail(e) => format!("bad build: {}", e),
        BuildResult::Panic(e) => format!("bad panic: {}", e),
    };

    println!("{}", s);

    let mut file = File::create(result_file).unwrap();
    let _ = writeln!(file, "{}", s);
}

fn bad(entry: &DirEntry) -> bool {
    entry.file_name()
         .to_str()
         .map(|s| s.starts_with(".") || s.ends_with(".json"))
         .unwrap_or(false)
}

fn config(work_dir: &Path, out: Box<Write + Send>, err: Box<Write + Send>) -> Config {
    let work_dir = env::current_dir().unwrap().join(work_dir);
    let config = ShellConfig {
        color_config: ColorConfig::Never,
        tty: false,
    };
    let out = Shell::create(Box::new(out), config);
    let err = Shell::create(Box::new(err), config);
    Config::new(MultiShell::new(out, err, Verbosity::Verbose),
                work_dir.to_owned(),
                work_dir.join("cargo-home")).unwrap()
}

enum BuildResult {
    Success,
    TestFail(String),
    BuildFail(String),
    Panic(String),
}

fn build(work_dir: &Path, out_dir: &Path, krate: &str) -> BuildResult {

    use std::panic::catch_unwind;

    let r = catch_unwind(|| {
        build_(work_dir, out_dir, krate)
    });

    match r {
        Ok(r) => r,
        Err(e) => {
            if let Some(e) = e.downcast_ref::<String>() {
                BuildResult::Panic(e.to_string())
            } else {
                BuildResult::Panic("some panic".to_string())
            }
        }
    }
}

fn build_(work_dir: &Path, stdio_dir: &Path, krate: &str) -> BuildResult {

    let out = File::create(stdio_dir.join("stdout")).unwrap();
    let err = File::create(stdio_dir.join("stderr")).unwrap();
    let config = config(&work_dir, Box::new(out), Box::new(err));
    let id = SourceId::for_central(&config).unwrap();
    let mut src = RegistrySource::new(&id, &config);

    let dep = Dependency::parse(krate, None, &id).unwrap();
    let pkg = src.query(&dep).unwrap().iter().map(|v| v.package_id())
                 .max().cloned();
    let pkg = match pkg {
        Some(pkg) => pkg,
        None => {
            panic!("failed to find {}", krate);
        }
    };

    let pkg = match src.download(&pkg) {
        Ok(v) => v,
        Err(e) => {
            panic!("bad get pkg: {}: {}", pkg, e);
        }
    };

    let args = &["-Z".to_string(), "time-passes".to_string()];
    let opts = compiler_opts(&config, args);
    let res = ops::compile_pkg(&pkg, None, &opts);

    if let Err(e) = res {
        return BuildResult::BuildFail(format!("{}: {}", pkg, e));
    }

    let opts = &ops::TestOptions {
        compile_opts: compiler_opts(&config, &[]),
        no_run: false,
        no_fail_fast: false,
    };

    let res = ops::run_tests(pkg.manifest_path(), opts, &[]);

    if let Err(e) = res {
        return BuildResult::TestFail(format!("{}: {}", pkg, e));
    }

    BuildResult::Success
}

fn compiler_opts<'a>(config: &'a Config, args: &'a [String]) -> ops::CompileOptions<'a> {
    ops::CompileOptions {
        config: config,
        jobs: None,
        target: None,
        features: &[],
        no_default_features: false,
        spec: &[],
        filter: ops::CompileFilter::Only {
            lib: true,
            examples: &[],
            bins: &[],
            tests: &[],
            benches: &[],
        },
        exec_engine: None,
        release: true,
        mode: ops::CompileMode::Build,
        target_rustc_args: Some(args),
        target_rustdoc_args: None,
    }
}
