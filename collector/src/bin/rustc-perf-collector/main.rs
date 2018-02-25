#![recursion_limit = "1024"]

extern crate chrono;
#[macro_use]
extern crate clap;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
#[macro_use]
extern crate log;
extern crate rust_sysroot;
extern crate collector;
extern crate serde_json;
extern crate tempdir;
extern crate cargo_metadata;
extern crate serde;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate lazy_static;

mod errors {
    // Create the Error, ErrorKind, ResultExt, and Result types
    error_chain! {
        foreign_links {
            RustSysroot(::rust_sysroot::errors::Error);
            Serde(::serde_json::Error);
            Chrono(::chrono::ParseError);
            Io(::std::io::Error);
            Metadata(::cargo_metadata::Error);
            Utf8(::std::string::FromUtf8Error);
        }
    }
}

use errors::*;

quick_main!(run);

use std::fs;
use std::str;
use std::path::{Path, PathBuf};
use std::io::{stderr, stdout, Write};
use std::collections::BTreeMap;

use chrono::{DateTime, Utc};

use collector::{Commit, CommitData, Date};
use rust_sysroot::git::Commit as GitCommit;
use rust_sysroot::sysroot::Sysroot;

mod git;
mod execute;
mod outrepo;

use execute::Benchmark;

fn bench_commit(
    commit: &GitCommit,
    repo: Option<&outrepo::Repo>,
    sysroot: Sysroot,
    benchmarks: &[Benchmark],
    iterations: usize,
) -> CommitData {
    info!(
        "benchmarking commit {} ({}) for triple {}",
        commit.sha,
        commit.date,
        sysroot.triple
    );

    let existing_data = repo.and_then(|r| r.load_commit_data(&commit, &sysroot.triple).ok());

    let mut results = BTreeMap::new();
    if let Some(ref data) = existing_data {
        for benchmark in benchmarks {
            if let Some(result) = data.benchmarks.get(&benchmark.name) {
                results.insert(benchmark.name.clone(), result.clone());
            }
        }
    }
    for benchmark in benchmarks {
        if results.contains_key(&benchmark.name) {
            continue;
        }

        let result = benchmark.run(&sysroot, iterations);

        if let Err(ref s) = result {
            info!(
                "failure to benchmark {}, recorded: {}",
                benchmark.name,
                s
            );
        }

        results.insert(benchmark.name.clone(), result.map_err(|e| format!("{:?}", e)));
        info!("{} benchmarks left", benchmarks.len() - results.len());
    }

    CommitData {
        commit: Commit {
            sha: commit.sha.clone(),
            date: Date(commit.date),
        },
        triple: sysroot.triple.clone(),
        benchmarks: results,
    }
}

fn get_benchmarks(benchmark_dir: &Path, filter: Option<&str>) -> Result<Vec<Benchmark>> {
    let mut benchmarks = Vec::new();
    for entry in fs::read_dir(benchmark_dir).chain_err(|| "failed to list benchmarks")? {
        let entry = entry?;
        let path = entry.path();
        let name = match entry.file_name().into_string() {
            Ok(s) => s,
            Err(e) => bail!("non-utf8 benchmark name: {:?}", e),
        };

        if path.ends_with(".git") || path.ends_with("scripts") || !entry.file_type()?.is_dir() {
            debug!("benchmark {} - ignored", name);
            continue;
        }

        if let Some(filter) = filter {
            if !name.contains(filter) {
                debug!("benchmark {} - filtered", name);
                continue;
            }
        }

        debug!("benchmark {} - registered", name);
        benchmarks.push(Benchmark::new(name, path)?);
    }
    benchmarks.sort_by_key(|benchmark| benchmark.name.clone());
    Ok(benchmarks)
}

fn process_commit(
    repo: &outrepo::Repo,
    commit: &GitCommit,
    benchmarks: &[Benchmark],
) -> Result<()> {
    let sysroot = Sysroot::install(commit, "x86_64-unknown-linux-gnu", false, false)?;
    repo.success(&bench_commit(commit, Some(repo), sysroot, benchmarks, 3))
}

fn process_retries(
    commits: &[GitCommit],
    repo: &mut outrepo::Repo,
    benchmarks: &[Benchmark],
) -> Result<()> {
    while let Some(retry) = repo.next_retry() {
        info!("retrying {}", retry);
        let commit = commits.iter().find(|commit| commit.sha == retry).unwrap();
        process_commit(repo, commit, benchmarks)?;
    }
    Ok(())
}

fn process_commits(
    commits: &[GitCommit],
    repo: &outrepo::Repo,
    benchmarks: &[Benchmark],
) -> Result<()> {
    println!("processing commits");
    if !commits.is_empty() {
        let to_process =
            repo.find_missing_commits(commits, benchmarks, "x86_64-unknown-linux-gnu")?;
        // take 3 from the end -- this means that for each bors commit (which takes ~3 hours) we
        // test 3, which should allow us to eventually test all commits, but also keep up with the
        // latest rustc
        for commit in to_process.iter().rev().take(3) {
            if let Err(err) = process_commit(repo, &commit, &benchmarks) {
                repo.write_broken_commit(commit, err)?;
            }
        }
    } else {
        info!("Nothing to do; no commits.");
    }
    Ok(())
}

fn run() -> Result<i32> {
    env_logger::init();
    git::fetch_rust(Path::new("rust.git"))?;

    let matches = clap_app!(rustc_perf_collector =>
       (version: "0.1")
       (author: "The Rust Compiler Team")
       (about: "Collects Rust performance data")
       (@arg filter: --filter +takes_value "Run only benchmarks that contain this")
       (@arg sync_git: --("sync-git") "Synchronize repository with remote")
       (@arg output_repo: --("output-repo") +required +takes_value "Repository to output to")
       (@subcommand process =>
           (about: "syncs to git and collects performance data for all versions")
       )
       (@subcommand bench_commit =>
           (about: "benchmark a bors merge from AWS and output data to stdout")
           (@arg COMMIT: +required +takes_value "Commit hash to bench")
       )
       (@subcommand bench_local =>
           (about: "benchmark a bors merge from AWS and output data to stdout")
           (@arg COMMIT: --commit +required +takes_value "Commit hash to associate benchmark results with")
           (@arg DATE: --date +required +takes_value "Date to associate benchmark result with, in the RFC3339 \"YYYY-MM-DDTHH:MM:SS-HH:MM\" format.")
           (@arg RUSTC: +required +takes_value "the path to the local rustc to benchmark")
       )
       (@subcommand remove_errs =>
           (about: "remove errored data")
       )
       (@subcommand remove_benchmark =>
           (about: "remove data for a benchmark")
           (@arg BENCHMARK: --benchmark +required +takes_value "benchmark name to remove data for")
       )
       (@subcommand test_benchmarks =>
           (about: "test some set of benchmarks, controlled by --filter")
       )
    ).get_matches();
    let benchmark_dir = PathBuf::from("collector/benchmarks");
    let filter = matches.value_of("filter");
    let benchmarks = get_benchmarks(&benchmark_dir, filter)?;
    let use_remote = matches.is_present("sync_git");
    let out_repo = PathBuf::from(matches.value_of_os("output_repo").unwrap());
    let mut out_repo = outrepo::Repo::open(out_repo, use_remote)?;

    let commits = rust_sysroot::get_commits(rust_sysroot::EPOCH_COMMIT, "master")?;

    match matches.subcommand() {
        ("test_benchmarks", Some(_)) => {
            let to_process =
                out_repo.find_missing_commits(&commits, &benchmarks, "x86_64-unknown-linux-gnu")?;
            // take 3 from the end -- this means that for each bors commit (which takes ~3 hours) we
            // test 3, which should allow us to eventually test all commits, but also keep up with the
            // latest rustc
            if let Some(commit) = to_process.last() {
                let sysroot = Sysroot::install(commit, "x86_64-unknown-linux-gnu", false, false)?;
                bench_commit(commit, None, sysroot, &benchmarks, 1);
            }
            Ok(0)
        }
        ("process", Some(_)) => {
            process_retries(&commits, &mut out_repo, &benchmarks)?;
            process_commits(&commits, &out_repo, &benchmarks)?;
            Ok(0)
        }
        ("bench_commit", Some(sub_m)) => {
            let commit = sub_m.value_of("COMMIT").unwrap();
            let commit = commits.iter().find(|c| c.sha == commit).cloned().unwrap_or_else(|| {
                warn!("utilizing fake commit!");
                rust_sysroot::git::Commit {
                    sha: commit.to_string(),
                    date: Date::ymd_hms(2000, 01, 01, 0, 0, 0).0,
                    summary: String::new(),
                }
            });
            process_commit(&out_repo, &commit, &benchmarks)?;
            Ok(0)
        }
        ("bench_local", Some(sub_m)) => {
            let commit = sub_m.value_of("COMMIT").unwrap();
            let date = sub_m.value_of("DATE").unwrap();
            let rustc = sub_m.value_of("RUSTC").unwrap();
            let commit = GitCommit {
                sha: commit.to_string(),
                date: DateTime::parse_from_rfc3339(date)?.with_timezone(&Utc),
                summary: String::new(),
            };
            let sysroot = Sysroot::with_local_rustc(
                &commit,
                rustc,
                "x86_64-unknown-linux-gnu",
                false,
                false,
            )?;
            let result = bench_commit(&commit, None, sysroot, &benchmarks, 3);
            serde_json::to_writer(&mut stdout(), &result)?;
            Ok(0)
        }
        ("remove_errs", Some(_)) => {
            for commit in &commits {
                if let Ok(mut data) = out_repo.load_commit_data(&commit, "x86_64-unknown-linux-gnu")
                {
                    let benchmarks = data.benchmarks
                        .into_iter()
                        .filter(|&(_, ref v)| v.is_ok())
                        .collect();
                    data.benchmarks = benchmarks;
                    out_repo.add_commit_data(&data)?;
                }
            }
            Ok(0)
        }
        ("remove_benchmark", Some(sub_m)) => {
            let benchmark = sub_m.value_of("BENCHMARK").unwrap();
            for commit in &commits {
                if let Ok(mut data) = out_repo.load_commit_data(&commit, "x86_64-unknown-linux-gnu")
                {
                    if data.benchmarks.remove(&*benchmark).is_none() {
                        warn!("could not remove {} from {}", benchmark, commit.sha);
                    }
                    out_repo.add_commit_data(&data)?;
                }
            }
            Ok(0)
        }
        _ => {
            let _ = writeln!(stderr(), "{}", matches.usage());
            Ok(2)
        }
    }
}
