//! Host narrowing of native recursive safety ceilings; never durable run state.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(try_from = "LimitsWire")]
pub struct KernelDataLimits {
    document_depth: usize,
    document_json_depth: usize,
    decode_depth: u64,
    state_json_depth: usize,
    value_depth: usize,
}
impl KernelDataLimits {
    /// Existing maxima: document 64, document JSON 352, binary value decode
    /// 512, parked JSON 512, run values 128. Native recursive readers, tree
    /// conversion and value copying require these independent stack ceilings.
    pub const fn standard() -> Self {
        Self {
            document_depth: 64,
            document_json_depth: 352,
            decode_depth: 512,
            state_json_depth: 512,
            value_depth: 128,
        }
    }
    pub fn new(
        document_depth: usize,
        document_json_depth: usize,
        decode_depth: u64,
        state_json_depth: usize,
        value_depth: usize,
    ) -> Option<Self> {
        (document_depth <= 64
            && document_json_depth <= 352
            && decode_depth <= 512
            && state_json_depth <= 512
            && value_depth <= 128
            && value_depth > 0)
            .then_some(Self {
                document_depth,
                document_json_depth,
                decode_depth,
                state_json_depth,
                value_depth,
            })
    }
    pub const fn document_depth(self) -> usize {
        self.document_depth
    }
    pub const fn document_json_depth(self) -> usize {
        self.document_json_depth
    }
    pub const fn decode_depth(self) -> u64 {
        self.decode_depth
    }
    pub const fn state_json_depth(self) -> usize {
        self.state_json_depth
    }
    pub const fn value_depth(self) -> usize {
        self.value_depth
    }
}
impl Default for KernelDataLimits {
    fn default() -> Self {
        Self::standard()
    }
}
#[derive(Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct LimitsWire {
    #[schemars(range(max = 64))]
    document_depth: usize,
    #[schemars(range(max = 352))]
    document_json_depth: usize,
    #[schemars(range(max = 512))]
    decode_depth: u64,
    #[schemars(range(max = 512))]
    state_json_depth: usize,
    #[schemars(range(min = 1, max = 128))]
    value_depth: usize,
}
impl TryFrom<LimitsWire> for KernelDataLimits {
    type Error = &'static str;
    fn try_from(w: LimitsWire) -> Result<Self, Self::Error> {
        Self::new(w.document_depth, w.document_json_depth, w.decode_depth, w.state_json_depth, w.value_depth)
            .ok_or("kernel data limits exceed native safety ceilings (64/352/512/512/128), or value depth is zero")
    }
}
