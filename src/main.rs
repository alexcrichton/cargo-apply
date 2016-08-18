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
use std::process::Command;
use std::time::Instant;

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
    // This is a bit crafty. We want to invoke ourselves, and we want
    // to do it using a secret option `--recurse` which must come first.
    let args: Vec<String> = env::args().collect();
    if args[1] == "--recurse" {
        recursive_invocation(args);
    } else {
        base_invocation(args);
    }
}

fn base_invocation(arg_strings: Vec<String>) {
    // Parse the argument strings.
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.argv(&arg_strings).decode())
        .unwrap_or_else(|e| e.exit());

    // Extract out the configuration options. These will
    // be all arguments but the last N.
    let limit = arg_strings.len() - args.arg_package_name.len();
    let config_options = &arg_strings[..limit];

    // Compute the paths to various important directories and create
    // them.
    let ref work_dir = PathBuf::from(&args.flag_out);
    let ref index_path = work_dir.join("index");
    let ref tmp_index_path = work_dir.join(".index");
    let ref cargo_dir = work_dir.join(".cargo");
    let ref output_dir = work_dir.join("output");
    fs::create_dir_all(cargo_dir).unwrap();
    fs::create_dir_all(output_dir).unwrap();

    // Initialize the index and load it via git.
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

    // Create a cargo configuration that directs all builds into a
    // shared directory. This allows us to re-use work.
    let cargo_config = format!("
        [build]
        target-dir = './target'
    ");
    File::create(cargo_dir.join("config"))
        .unwrap()
        .write_all(cargo_config.as_bytes())
        .unwrap();

    // Assemble the full list of crates we want to process.
    let crates: Vec<_> = match assemble_crate_names(&args.arg_package_name, &index_path) {
        Ok(v) => v,
        Err(()) => return,
    };

    // Iterate over the crates. For each one, we will recursively
    // spawn ourselves in a separate process, redirecting the stdout
    // and stderr into files. The return code of this recursive
    // process also tells us what happened.
    for krate in crates {
        match process_crate(config_options, &output_dir, &args, &krate) {
            Ok(()) => { }
            Err(err) => {
                println!("{}: error `{}`", krate, err);
            }
        }
    }
}

fn process_crate(config_options: &[String],
                 output_dir: &Path,
                 args: &Args,
                 krate: &KrateName)
                 -> Result<(), Box<Error>> {
    let krate_str = krate.to_string();
    let ref krate_dir = output_dir.join(&krate_str);

    fs::create_dir_all(krate_dir)?;

    let out_path = krate_dir.join("stdout");
    let err_path = krate_dir.join("stderr");
    let result_path = krate_dir.join("results.txt");

    // Skip if a result file already exists.
    if fs::metadata(&result_path).is_ok() {
        if !args.flag_force {
            println!("{}: re-using existing results", krate);
            return Ok(());
        }
    }

    println!("{}: processing", krate);

    // Delete old files if they exist.
    let _ = fs::remove_file(&out_path);
    let _ = fs::remove_file(&err_path);
    let _ = fs::remove_file(&result_path);

    // Recursively invoke ourselves with `--recurse`,
    // the configuration options, and the krate to process.
    let output = Command::new(env::current_exe()?)
        .arg("--recurse")
        .args(config_options)
        .arg(&krate_str)
        .output()?;

    // Save the output into stdio/stderr. Create result file last.
    let mut out_file = File::create(out_path)?;
    out_file.write_all(&output.stdout)?;
    let mut err_file = File::create(err_path)?;
    err_file.write_all(&output.stderr)?;
    let mut result_file = File::create(result_path)?;
    write!(result_file, "exit code `{:?}`", output.status)?;
    if !output.status.success() {
        println!("{}: completed with error code {:?}", krate, output.status);
    }

    Ok(())
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

fn recursive_invocation(mut arg_strings: Vec<String>) {
    arg_strings.remove(1); // drop the `--recurse` flag
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.argv(&arg_strings[1..]).decode())
        .unwrap_or_else(|e| e.exit());

    // FIXME not dry
    let ref work_dir = PathBuf::from(&args.flag_out);
    let ref index_path = work_dir.join("index");

    // We expect exactly one crate name.
    let krate_names = assemble_crate_names(&args.arg_package_name, &index_path).unwrap();
    for krate in krate_names {
        build_crate(work_dir, krate.clone(), &args).unwrap();
    }
}

fn build_crate(work_dir: &Path,
               krate: KrateName,
               args: &Args)
               -> Result<(), Box<Error>> {
    let config = config(&work_dir, Box::new(io::stdout()), Box::new(io::stderr()));
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

    {
        println!("building: {}", krate);
        let start = Instant::now();
        ops::compile_pkg(&pkg, None, &opts)?;
        let compile_time = start.elapsed();
        println!("krate `{}` built in {:?}", krate, compile_time);
    }

    if args.flag_test {
        println!("testing: {}", krate);
        let opts = &test_opts(&config, &[], args);
        let start = Instant::now();
        ops::run_tests(pkg.manifest_path(), opts, &[])?;
        let test_time = start.elapsed();
        println!("krate `{}` tested in {:?}", krate, test_time);
    }

    if args.flag_bench {
        let opts = &test_opts(&config, &[], args);

        let start = Instant::now();
        println!("benchmarking: {}", krate);
        ops::run_benches(pkg.manifest_path(), &opts, &[])?;
        let bench_time = start.elapsed();
        println!("krate `{}` benchmarked in {:?}", krate, bench_time);
    }

    Ok(())
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

fn assemble_crate_names(arg_package_names: &[String],
                        index_path: &Path)
                        -> Result<Vec<KrateName>, ()> {
    if arg_package_names.iter().any(|s| s == "*") {
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
        let regex = Regex::new(r"\s*([^=\s]+)\s*(?:=\s*([^=\s]+))?").unwrap();
        arg_package_names.iter()
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
enum BuildError {
    NotInRegistry(KrateName),
    FailedToDownload(KrateName, Box<Error>),
}

impl fmt::Display for BuildError {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
        use BuildError::*;
        match *self {
            NotInRegistry(ref k) => write!(fmt, "crate `{}` not in registry", k),
            FailedToDownload(ref k, ref e) => {
                write!(fmt, "crate `{}` failed to download: {}", k, e)
            }
        }
    }
}

impl Error for BuildError {
    fn description(&self) -> &str {
        use BuildError::*;
        match *self {
            NotInRegistry(..) => "not in registry",
            FailedToDownload(..) => "failed to download",
        }
    }

    fn cause(&self) -> Option<&Error> {
        use BuildError::*;
        match *self {
            NotInRegistry(..) => None,
            FailedToDownload(_, ref e) => Some(&**e),
        }
    }
}
