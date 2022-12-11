use serde::{Deserialize, Serialize};

/// Which algorithm to run and benchmark, along with algorithm-specific parameters.
#[derive(Serialize, Deserialize, Debug)]
pub enum Algorithm {
    BlockAligner(BlockAlignerParams),
    ParasailStriped(ParasailStripedParams),
    // Add more algorithms here!
}

#[derive(Serialize, Deserialize, Debug)]
pub struct BlockAlignerParams {
    pub min_size: usize,
    pub max_size: usize,
}

pub type ParasailStripedParams = ();
