mod config;
mod stats;

use chrono::Timelike;
use clap::Parser;
use core_affinity;
use serde_json;
use serde_yaml;
use std::fs;
use std::io::prelude::*;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use wait4::{ResUse, Wait4};

use pa_bench_types::*;

use config::*;

#[derive(Debug, Parser)]
#[command(author, version, about)]
struct Args {
    /// Path to an experiment yaml file.
    experiment: PathBuf,

    /// Path to the output json file. By default mirrors the `experiments` dir in `results`.
    results: Option<PathBuf>,

    /// Path to the data directory.
    #[arg(short, long, default_value = "evals/data")]
    data_dir: PathBuf,

    /// Path to the logs directory.
    ///
    /// Results of all runs are stored here.
    #[arg(short, long, default_value = "evals/results/.log")]
    logs_dir: PathBuf,

    /// Path to the runner binary. Uses $CARGO_MANIFEST_DIR/../target/release/runner by default.
    #[arg(long)]
    runner: Option<PathBuf>,

    /// Time limit. Defaults to value in experiment yaml or 1m.
    #[arg(short, long, value_parser = parse_duration::parse)]
    time_limit: Option<Duration>,

    /// Memory limit. Defaults to value in experiment yaml or 1GiB.
    #[arg(short, long, value_parser = parse_bytes)]
    mem_limit: Option<Bytes>,

    // process niceness. <0 for higher priority.
    #[arg(long)]
    nice: Option<i32>,

    /// Number of parallel jobs to use.
    ///
    /// Jobs are pinned to separate cores.
    /// The number of jobs is capped to the total number of cores minus 1.
    #[arg(short = 'j', long)]
    num_jobs: Option<usize>,

    /// Show stderr of runner process.
    #[arg(long)]
    stderr: bool,

    /// Skip jobs already present in the results file.
    ///
    /// This skips any already-present job that either succeeded or had at least
    /// as many resources as the new job.
    #[arg(short, long)]
    incremental: bool,

    /// In combination with --incremental, rerun all failed jobs.
    ///
    /// This also reruns jobs that had at least as many resources. Useful when code changed.
    #[arg(short, long)]
    rerun_failed: bool,

    /// Verbose runner outputs.
    #[arg(short, long)]
    verbose: bool,

    /// Ignore the existing results json and regenerate datasets.
    #[arg(long)]
    force_rerun: bool,
}

fn main() {
    let mut args = Args::parse();
    if args.runner.is_none() {
        let dir = std::env::var("CARGO_MANIFEST_DIR")
            .expect("Neither --runner nor CARGO_MANIFEST_DIR env var is set.");
        args.runner = Some(Path::new(&dir).join("../target/release/runner"));
    }

    assert!(
        args.runner.as_ref().unwrap().exists(),
        "{} does not exist!",
        args.runner.unwrap().display()
    );

    let experiment_yaml =
        fs::read_to_string(&args.experiment).expect("Failed to read jobs generator:");
    let experiments: Experiments =
        serde_yaml::from_str(&experiment_yaml).expect("Failed to parse jobs generator yaml:");

    let results_path = args.results.unwrap_or_else(|| {
        // Mirror the structure of experiments in results.
        // To be precise: replace the last directory named "experiments" by "results".
        let mut found = false;
        args.experiment
            .with_extension("json")
            .iter()
            .rev()
            .map(|c| {
                if c == "experiments" && !found {
                    found = true;
                    "results"
                } else {
                    c.to_str().unwrap()
                }
            })
            .rev()
            .collect()
    });
    let mut jobs = experiments.generate(
        &args.data_dir,
        args.force_rerun,
        args.time_limit,
        args.mem_limit,
    );
    eprintln!("Generated {} jobs.", jobs.len());

    // Read the existing results file.
    let mut existing_job_results: Vec<JobResult> = if !args.force_rerun && results_path.is_file() {
        serde_json::from_str(
            &fs::read_to_string(&results_path).expect("Error reading existing results file"),
        )
        .expect("Error parsing results json file")
    } else {
        vec![]
    };

    // Skip jobs that succeeded before, or were attempted with at least as many resources.
    if args.incremental {
        eprintln!("Existing jobs: {}", existing_job_results.len());
        let num_jobs_before = jobs.len();
        jobs.retain(|(job, _stats)| {
            existing_job_results
                .iter()
                .find(|existing_job| {
                    existing_job.job.is_same_as(job)
                        && (existing_job.output.is_ok()
                            || (!args.rerun_failed
                                && existing_job.job.has_more_resources_than(job)))
                })
                .is_none()
        });
        eprintln!("Reused jobs: {}", num_jobs_before - jobs.len());
        eprintln!("Running {} jobs...", jobs.len());
    };

    let runner_cores = if let Some(num_jobs) = args.num_jobs {
        let mut cores = core_affinity::get_core_ids()
            .unwrap()
            .into_iter()
            // NOTE: This assumes that virtual cores 0 and n/2 are on the same
            // physical core, in case hyperthreading is enabled.
            // TODO(ragnar): Is it better to spread the load over non-adjacent
            // physical cores? Unclear to me.
            .take(num_jobs + 1);

        // Reserve one core for the orchestrator.
        let orchestrator_core = cores.next().unwrap();
        core_affinity::set_for_current(orchestrator_core);

        // Remaining (up to) #processes cores are for runners.
        Some(cores.map(|c| c.id).collect())
    } else {
        None
    };

    let job_results = run_with_threads(
        &args.runner.unwrap(),
        jobs,
        runner_cores,
        args.nice,
        args.stderr,
        args.verbose,
    );

    {
        let logs_path = args.logs_dir.join(format!(
            "{}_{}.json",
            args.experiment.file_stem().unwrap().to_str().unwrap(),
            chrono::Local::now()
                .with_nanosecond(0)
                .unwrap()
                .to_rfc3339()
        ));
        // Write results to persistent log.
        fs::create_dir_all(args.logs_dir).unwrap();
        fs::write(&logs_path, &serde_json::to_string(&job_results).unwrap())
            .expect(&format!("Failed to write logs to {}", logs_path.display()));
    }

    // Remove jobs that were run from existing results.
    existing_job_results.retain(|existing_job| {
        job_results
            .iter()
            .find(|job| job.job.is_same_as(&existing_job.job))
            .is_none()
    });

    // Append new results to existing results.
    existing_job_results.extend(job_results);
    let mut job_results = existing_job_results;

    if let Some(dir) = results_path.parent() {
        fs::create_dir_all(dir).unwrap();
    }
    eprintln!("Output: {}", results_path.display());
    fs::write(&results_path, &serde_json::to_string(&job_results).unwrap()).expect(&format!(
        "Failed to write results to {}",
        results_path.display()
    ));

    verify_costs(&mut job_results);
}

/// Verify costs for exact algorithms and count correct costs for approximate algorithms.
fn verify_costs(results: &mut Vec<JobResult>) {
    // Ensure exact algorithms are first in results.
    results.sort_by_key(|res| !res.output.as_ref().map(|o| o.is_exact).unwrap_or(false));

    for i in 0..results.len() {
        let (earlier_results, result) = results.split_at_mut(i);
        let result = &mut result[0];

        let Ok(output) = result.output.as_mut() else {
            // Nothing to do for failed jobs.
            continue;
        };

        // Find the first exact job with the same input and compare costs.
        for reference_result in earlier_results {
            if !reference_result.job.same_input(&result.job) {
                continue;
            }
            let Ok(reference_output) = reference_result.output.as_ref() else {
                continue;
            };
            if !reference_output.is_exact {
                continue;
            }
            assert_eq!(
                output.costs.len(),
                reference_output.costs.len(),
                "\nDifferent number of costs!\nJob 1: {:?}\nJob 2: {:?}\nLen costs 1: {:?}\nLen costs 2: {:?}",
                result.job,
                reference_result.job,
                output.costs.len(),
                reference_output.costs.len(),
            );
            if output.is_exact {
                // For exact jobs, simply check they give the same result.
                assert_eq!(
                            output.costs,
                            reference_output.costs,
                            "\nIncorrect costs of exact algorithms!\nJob 1: {:?}\nJob 2: {:?}\nCosts 1: {:?}\nCosts 2: {:?}",
                            result.job,
                            reference_result.job,
                            output.costs,
                            reference_output.costs,
                        );
            } else {
                // For inexact jobs, add the correct ones and the fraction of correct results.
                output.exact_costs = Some(reference_output.costs.clone());
                let num_correct = output
                    .costs
                    .iter()
                    .zip(&reference_output.costs)
                    .filter(|(&a, &b)| a == b)
                    .count();
                output.p_correct = Some((num_correct as f32) / (output.costs.len() as f32));
            }
        }
    }
}

fn run_with_threads(
    runner: &Path,
    jobs: Vec<(Job, AlignStats)>,
    cores: Option<Vec<usize>>,
    nice: Option<i32>,
    show_stderr: bool,
    verbose: bool,
) -> Vec<JobResult> {
    let num_jobs = jobs.len();
    let job_results = Mutex::new(Vec::<JobResult>::with_capacity(jobs.len()));
    let jobs_iter = Mutex::new(jobs.into_iter());

    // Make a `Vec<Option<usize>>` which defaults to `[None]`.
    let cores = cores
        .map(|cores| cores.into_iter().map(Some).collect())
        .unwrap_or(vec![None]);

    let running = Arc::new(Mutex::new(true));
    {
        let r = running.clone();
        ctrlc::set_handler(move || {
            eprintln!("Pressed Ctrl-C. Stopping running jobs.");
            *r.lock().unwrap() = false;
        })
        .expect("Error setting Ctrl-C handler");
    }

    #[derive(Default)]
    struct Counts {
        done: usize,
        success: usize,
        unsupported: usize,
        skipped: usize,
        failed: usize,
    }

    let counts = Mutex::new(Counts::default());

    thread::scope(|scope| {
        for id in &cores {
            scope.spawn(|| {
                loop {
                    let Some((job, stats)) = jobs_iter.lock().unwrap().next() else {
                        break;
                    };
                    if !*running.lock().unwrap() {
                        break;
                    }
                    // If a smaller job for the same algorithm failed, skip it.
                    let mut skip = false;
                    if job.dataset.is_generated() {
                        for prev in job_results.lock().unwrap().iter() {
                            if prev.output.is_err()
                                && prev.job.dataset.is_generated()
                                && job.is_larger(&prev.job)
                            {
                                skip = true;
                                break;
                            }
                        }
                    }

                    let job_result = if skip {
                        JobResult {
                            job,
                            stats,
                            resources: ResourceUsage::default(),
                            output: Err(JobError::Skipped),
                        }
                    } else {
                        run_job(runner, job, stats, *id, nice, show_stderr, verbose)
                    };

                    let mut counts = counts.lock().unwrap();
                    counts.done += 1;
                    if job_result.output.is_ok() {
                        counts.success += 1;
                    } else if skip {
                        counts.skipped += 1;
                    } else if *job_result.output.as_ref().unwrap_err() == JobError::Unsupported {
                        counts.unsupported += 1;
                    } else if *job_result.output.as_ref().unwrap_err() != JobError::Interrupted {
                        counts.failed += 1;
                        eprintln!("\n Failed job:\n{}\n Result: {:?}\n {:?}\n", serde_json::to_string(&job_result.job).unwrap(), job_result.output, job_result.resources);
                    };
                    let Counts {
                        done,
                        success,
                        unsupported,
                        skipped,
                        failed,
                    } = *counts;
                    eprint!("\r Processed: {done:3} / {num_jobs:3}. Success {success:3}, Unsupported {unsupported:3}, Failed {failed:3}, Skipped {skipped}");

                    // If the orchestrator was aborted, do not push failing job results.
                    if job_result.output.is_ok() || *running.lock().unwrap() {
                        job_results.lock().unwrap().push(job_result);
                    }
                }
            });
        }
    });
    // Print a newline after the last count message
    eprintln!();

    job_results.into_inner().unwrap()
}

fn run_job(
    runner: &Path,
    job: Job,
    stats: AlignStats,
    core_id: Option<usize>,
    nice: Option<i32>,
    show_stderr: bool,
    verbose: bool,
) -> JobResult {
    let mut cmd = Command::new(runner);
    if let Some(id) = core_id {
        cmd.arg("--pin-core-id").arg(id.to_string());
    }
    if let Some(nice) = nice {
        // negative numbers need to be passed with =.
        cmd.arg(format!("--nice={nice}"));
    }
    if verbose {
        cmd.arg("--verbose");
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
    if !show_stderr {
        cmd.stderr(Stdio::null());
    }
    let mut child = cmd.spawn().unwrap();

    {
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(&serde_json::to_vec(&job).unwrap()).unwrap();
    }

    let start = Instant::now();
    let ResUse { status, rusage } = child.wait4().unwrap();
    let walltime = start.elapsed().as_secs_f32();
    let mut stdout = Vec::new();
    child.stdout.unwrap().read_to_end(&mut stdout).unwrap();

    let resources = ResourceUsage {
        walltime,
        usertime: rusage.utime.as_secs_f32(),
        systemtime: rusage.stime.as_secs_f32(),
        maxrss: rusage.maxrss,
    };

    if status.success() {
        JobResult {
            job,
            stats,
            resources,
            output: Ok(serde_json::from_slice(&stdout).expect("Error reading output json:")),
        }
    } else {
        if show_stderr {
            if let Some(code) = status.signal() {
                if code == 24 {
                    eprintln!("Time limit exceeded for {job:?}");
                }
            }
        }
        let err = if let Some(signal) = status.signal() {
            match signal {
                2 => JobError::Interrupted,
                6 => JobError::MemoryLimit,
                9 => JobError::Timeout,
                signal => JobError::Signal(signal),
            }
        } else if let Some(code) = status.code() {
            match code {
                101 => JobError::Panic,
                102 => JobError::Unsupported,
                code => JobError::ExitCode(code),
            }
        } else {
            panic!("Unknown exit type {:?}", status);
        };
        JobResult {
            job,
            stats,
            resources,
            output: Err(err),
        }
    }
}
