use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use pa_generate::*;
use pa_types::*;

mod algorithms;
pub use algorithms::*;

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub struct GeneratedDataset {
    pub prefix: PathBuf,
    pub seed: u64,
    pub error_model: ErrorModel,
    pub error_rate: f32,
    pub length: usize,
    pub total_size: usize,
    pub pattern_length: Option<usize>,
}

impl GeneratedDataset {
    pub fn is_larger_than(&self, o: &Self) -> bool {
        self.error_model == o.error_model
            && self.pattern_length == o.pattern_length
            && self.error_rate >= o.error_rate
            && self.length >= o.length
            && self.total_size >= o.total_size
    }

    pub fn name(&self) -> String {
        format!(
            "{:?}-t{}-n{}-e{}.seq",
            self.error_model, self.total_size, self.length, self.error_rate
        )
    }

    pub fn path(&self) -> PathBuf {
        self.prefix.join(self.name())
    }

    pub fn to_generator(&self) -> DatasetGenerator {
        let Self {
            seed,
            error_model,
            error_rate,
            length,
            total_size,
            pattern_length,
            ..
        } = *self;
        DatasetGenerator {
            settings: SeqPairGenerator {
                length,
                error_rate,
                error_model,
                pattern_length,
            },
            seed: Some(seed),
            cnt: None,
            size: Some(total_size),
        }
    }
}

/// A dataset can be specified in several ways.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum Dataset {
    /// Options to deterministically generate a dataset.
    Generated(GeneratedDataset),
    /// Path to a .seq file.
    File(PathBuf),
    /// The data itself.
    /// NOTE: Only use this for testing small inputs.
    Data(Vec<(String, String)>),
}

impl Dataset {
    #[must_use]
    pub fn is_generated(&self) -> bool {
        matches!(self, Self::Generated(..))
    }
}

/// An alignment job: a single task for the runner to execute and benchmark.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Job {
    /// The maximum cpu time in seconds for the job.
    /// E.g. 1h. Parsed using parse_duration::parse.
    /// Set using RLIMIT_CPU.
    pub time_limit: u64,
    /// The maximum total memory usage for the job.
    /// Includes startup overhead, which is excluded from the measured memory usage.
    /// Set using RLIMIT_DATA.
    pub mem_limit: Bytes,
    /// Path to a `.seq` file.
    pub dataset: Dataset,
    /// The cost model to use.
    pub costs: CostModel,
    /// Return the full alignment/cigar?
    pub traceback: bool,
    /// The algorithm/parameters to use.
    pub algo: AlgorithmParams,
}

impl Job {
    /// Whether the jobs are the same, ignoring resources.
    pub fn is_same_as(&self, o: &Self) -> bool {
        self.dataset == o.dataset
            && self.costs == o.costs
            && self.traceback == o.traceback
            && self.algo == o.algo
    }

    /// Whether this job is larger than another job.
    /// Returns false when either job is not generated.
    pub fn has_more_resources_than(&self, o: &Self) -> bool {
        self.time_limit >= o.time_limit && self.mem_limit >= o.mem_limit
    }

    pub fn same_input(&self, o: &Self) -> bool {
        self.dataset == o.dataset && self.costs == o.costs
    }

    /// Whether this job is larger than another job.
    /// Returns false when either job is not generated.
    pub fn is_larger(&self, o: &Self) -> bool {
        let Dataset::Generated(self_args) = &self.dataset else {
            return false;
        };
        let Dataset::Generated(o_args) = &o.dataset else {
            return false;
        };
        // inputs must be the same
        self.costs == o.costs
            && self.algo == o.algo
            && self.traceback == o.traceback
            // resources must be less
            && self.time_limit <= o.time_limit
            && self.mem_limit <= o.mem_limit
            // parameters must be more
            && self_args.is_larger_than(o_args)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Measured {
    /// Runtime in seconds.
    pub runtime: f32,
    pub memory: Bytes,
    /// Formatted UTC time when run was started/ended.
    pub time_start: chrono::DateTime<chrono::Utc>,
    pub time_end: chrono::DateTime<chrono::Utc>,
    /// Cpu core running this process at start/end.
    pub cpu_start: Option<i32>,
    pub cpu_end: Option<i32>,
    /// Cpu frequency at start/end.
    pub cpu_freq_start: Option<f32>,
    pub cpu_freq_end: Option<f32>,
}

/// The output of an alignment job.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JobOutput {
    pub costs: Vec<Cost>,
    /// Corresponding exact costs if the job is approximate.
    pub exact_costs: Option<Vec<Cost>>,
    //pub cigars: Vec<Cigar>,
    pub is_exact: bool,
    /// Proportion of correct costs.
    pub p_correct: Option<f32>,
    pub measured: Measured,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum JobError {
    // Skipped because a smaller job failed before it.
    Skipped,
    // Interrupted by user, ie ctrl-C pressed.
    // SIGINT=2
    Interrupted,
    // Rust panic.
    // Exit code 101
    Panic,
    // Killed by kernel because cputime ran out.
    // SIGKILL=9
    Timeout,
    // Crashed because couldn't allocate.
    // SIGABRT=6
    MemoryLimit,
    Signal(i32),
}

/// The result of an alignment job, containing the input and output.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct JobResult {
    pub job: Job,
    // TODO(ragnar): Make this a result with a specific error type that indicates the failure reason.
    pub output: Result<JobOutput, (f32, JobError)>,
}
