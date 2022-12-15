use super::*;

use edlib_rs::edlibrs::*;

pub struct Edlib {
    config: EdlibAlignConfigRs<'static>,
}

impl AlignerParams for EdlibParams {
    type Aligner = Edlib;

    fn default(cm: CostModel, trace: bool, _max_len: usize) -> Self::Aligner {
        assert!(cm.is_unit());
        let mut config = EdlibAlignConfigRs::default();
        if trace {
            config.task = EdlibAlignTaskRs::EDLIB_TASK_PATH;
        }
        Self::Aligner { config }
    }
}

impl Aligner for Edlib {
    fn align(&mut self, a: Seq, b: Seq) -> (Cost, Option<Cigar>) {
        let result = edlibAlignRs(a, b, &self.config);
        assert!(result.status == EDLIB_STATUS_OK);
        let cost = result.getDistance();
        let cigar = result.getAlignment().map(|alignment| {
            Cigar::from_ops(alignment.into_iter().map(|op| match op {
                0 => CigarOp::Match,
                1 => CigarOp::Del,
                2 => CigarOp::Ins,
                3 => CigarOp::Sub,
                _ => panic!("Edlib should only return operations 0..=3."),
            }))
        });
        (cost, cigar)
    }
}
