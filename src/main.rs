#![feature(question_mark)]

extern crate cargo;
extern crate docopt;
extern crate git2;
extern crate walkdir;
extern crate libc;
extern crate regex;
extern crate rustc_serialize;

use std::error::Error;
use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::prelude::*;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cargo::core::{Source, SourceId, Registry, Dependency};
use cargo::ops;
use cargo::sources::RegistrySource;
use cargo::core::shell::{Shell, MultiShell, Verbosity, ShellConfig, ColorConfig};
use cargo::util::Config;
use docopt::Docopt;
use regex::Regex;
use walkdir::{WalkDir, DirEntry, WalkDirIterator};

// Write the Docopt usage string.
const USAGE: &'static str = r#"
Usage: cargo-apply [options] <package-name>...
       cargo-apply --help

Builds or tests the latest version of packages from crates.io, saving
timing information and other results. If the special package-name "*"
is used, we will test all packages. (Use `'*'` to prevent your shell
from expanding wildcards.)

WARNING: Building or testing packages from crates.io involves executing
arbitary code! Be wary.

Options:
    --out DIR              Output directory [default: work].
    -t, --test             Run tests.
    -b, --bench            Run benchmarks.
    --force                Delete results left-over from prior runs.
    --release              Use release mode instead of debug.
"#;

#[derive(Debug, RustcDecodable)]
struct Args {
    flag_out: String,
    flag_release: bool,
    flag_test: bool,
    flag_bench: bool,
    flag_force: bool,
    arg_package_name: Vec<String>,
}

#[derive(Clone, Debug)]
struct KrateName {
    name: String,
    version: Option<String>,
}

fn main() {
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.argv(env::args()).decode())
        .unwrap_or_else(|e| e.exit());

    let ref work_dir = PathBuf::from(&args.flag_out);
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
                                tmp_index_path)
            .unwrap();
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

    File::create(cargo_dir.join("config"))
        .unwrap()
        .write_all(cargo_config.as_bytes())
        .unwrap();

    let crates: Vec<_> = match assemble_crate_names(&args, &index_path) {
        Ok(v) => v,
        Err(()) => return,
    };

    for krate in crates {
        let krate_str = krate.to_string();
        let ref crate_result_dir = results_dir.join(&krate_str);
        let ref result_file = crate_result_dir.join("results.txt");

        fs::create_dir_all(crate_result_dir).unwrap();

        if fs::metadata(result_file).is_ok() {
            if !args.flag_force {
                println!("using existing results for: {}", krate);
                continue;
            } else {
                println!("deleting existing results for: {}", krate);
                fs::remove_file(&result_file).unwrap();
            }
        }

        println!("working on: {}", krate);

        let result = build(work_dir, stdio_dir, &krate, &args);
        report_result(&result_file, result);
    }
}

fn report_result(result_file: &Path, r: Result<Timing, Box<Error>>) {
    let s = match r {
        Ok(timing) => format!("{:?}", timing),
        Err(err) => format!("{}", err),
    };

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
                work_dir.join("cargo-home"))
        .unwrap()
}

fn build(work_dir: &Path,
         stdio_dir: &Path,
         krate: &KrateName,
         args: &Args)
         -> Result<Timing, Box<Error>> {

    use std::panic::catch_unwind;

    let krate_str = krate.to_string();
    let ref out_dir = stdio_dir.join(&krate_str);
    fs::create_dir_all(out_dir)?;

    let r = catch_unwind(|| build_(work_dir, out_dir, krate.clone(), args));

    match r {
        Ok(Ok(t)) => Ok(t),
        Ok(Err(e)) => Err(e),
        Err(e) => {
            if let Some(e) = e.downcast_ref::<String>() {
                Err(BuildError::Panic(krate.clone(), e.to_string()).into())
            } else {
                Err(BuildError::Panic(krate.clone(), "some panic".to_string()).into())
            }
        }
    }
}

fn build_(work_dir: &Path,
          stdio_dir: &Path,
          krate: KrateName,
          args: &Args)
          -> Result<Timing, Box<Error>> {
    let out = File::create(stdio_dir.join("stdout"))?;
    let err = File::create(stdio_dir.join("stderr"))?;
    let config = config(&work_dir, Box::new(out), Box::new(err));
    let id = SourceId::for_central(&config)?;
    let mut src = RegistrySource::new(&id, &config);

    let dep = Dependency::parse(&krate.name, krate.version.as_ref().map(|s| &s[..]), &id)?;
    let pkg = src.query(&dep)?
        .iter()
        .map(|v| v.package_id())
        .max()
        .cloned();
    let pkg = match pkg {
        Some(pkg) => pkg,
        None => return Err(BuildError::NotInRegistry(krate).into()),
    };

    let pkg = match src.download(&pkg) {
        Ok(v) => v,
        Err(e) => return Err(BuildError::FailedToDownload(krate, e.into()).into()),
    };

    let rustc_args = &[];
    let opts = compiler_opts(&config, rustc_args, args);

    println!("building: {}", krate);
    let compile_time = measure(|| Ok({ops::compile_pkg(&pkg, None, &opts)?;}))?;

    let mut test_time = None;
    if args.flag_test {
        println!("testing: {}", krate);
        let opts = &test_opts(&config, &[], args);
        test_time = Some(measure(|| Ok({ops::run_tests(pkg.manifest_path(), opts, &[])?;}))?)
    }

    let mut bench_time = None;
    if args.flag_bench {
        let opts = &test_opts(&config, &[], args);

        let start = Instant::now();
        println!("benchmarking: {}", krate);
        ops::run_benches(pkg.manifest_path(), &opts, &[])?;
        bench_time = Some(start.elapsed());
    }

    Ok(Timing {
        krate: krate,
        build: compile_time,
        test: test_time,
        bench: bench_time,
    })
}

fn measure<F>(op: F) -> Result<Duration, Box<Error>> where F: FnOnce() -> Result<(), Box<Error>> {
    let start = Instant::now();
    op()?;
    Ok(start.elapsed())
}

fn compiler_opts<'a>(config: &'a Config,
                     rustc_args: &'a [String],
                     args: &Args)
                     -> ops::CompileOptions<'a> {
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
        release: args.flag_release,
        mode: ops::CompileMode::Build,
        target_rustc_args: Some(rustc_args),
        target_rustdoc_args: None,
    }
}

fn test_opts<'a>(config: &'a Config,
                 rustc_args: &'a [String],
                 args: &Args)
                 -> ops::TestOptions<'a> {
    ops::TestOptions {
        compile_opts: compiler_opts(&config, rustc_args, &args),
        no_run: false,
        no_fail_fast: false,
    }
}

fn assemble_crate_names(args: &Args, index_path: &Path) -> Result<Vec<KrateName>, ()> {
    if args.arg_package_name.iter().any(|s| s == "*") {
        // assemble the list from the index
        Ok(WalkDir::new(index_path)
            .into_iter()
            .filter_entry(|e| !bad(e))
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.file_name().to_str().unwrap().to_string())
            .map(|f| {
                KrateName {
                    name: f,
                    version: None,
                }
            })
            .collect())
    } else {
        let regex = Regex::new(r"\s*([^=\s]+)\s*(?:=\s*[^=\s]+)?").unwrap();
        args.arg_package_name
            .iter()
            .map(|str| {
                match regex.captures(&str) {
                    Some(captures) => {
                        Ok(KrateName {
                            name: captures.at(1).unwrap().to_string(),
                            version: captures.at(2).map(|s| s.to_string()),
                        })
                    }
                    None => {
                        println!("invalid package name / version `{}`, try `foo` or `foo=0.1`",
                                 str);
                        Err(())
                    }
                }
            })
            .collect()
    }
}

impl fmt::Display for KrateName {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        if let Some(ref ver) = self.version {
            write!(fmt, "{}={}", self.name, ver)
        } else {
            write!(fmt, "{}", self.name)
        }
    }
}

#[derive(Debug)]
struct Timing {
    krate: KrateName,
    build: Duration,
    test: Option<Duration>,
    bench: Option<Duration>,
}

#[derive(Debug)]
enum BuildError {
    NotInRegistry(KrateName),
    FailedToDownload(KrateName, Box<Error>),
    Panic(KrateName, String),
}

impl fmt::Display for BuildError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use BuildError::*;
        match *self {
            NotInRegistry(ref k) => write!(fmt, "crate `{}` not in registry", k),
            FailedToDownload(ref k, ref e) => {
                write!(fmt, "crate `{}` failed to download: {}", k, e)
            }
            Panic(ref k, ref s) => write!(fmt, "crate `{}` encountered a misc panic: {}", k, s),
        }
    }
}

impl Error for BuildError {
    fn description(&self) -> &str {
        use BuildError::*;
        match *self {
            NotInRegistry(..) => "not in registry",
            FailedToDownload(..) => "failed to download",
            Panic(..) => "unexpected panic",
        }
    }

    fn cause(&self) -> Option<&Error> {
        use BuildError::*;
        match *self {
            NotInRegistry(..) |
            Panic(..) => None,
            FailedToDownload(_, ref e) => Some(&**e),
        }
    }
}
