use crate::spec::{base, StackProbeType, Target, TargetOptions};

pub(crate) fn target() -> Target {
    Target {
        llvm_target: "aarch64-unknown-linux-gnu".into(),
        metadata: crate::spec::TargetMetadata {
            description: None,
            tier: Some(3),
            host_tools: Some(false),
            std: None, // ?
        },
        pointer_width: 64,
        data_layout: "e-m:e-i8:8:32-i16:16:32-i64:64-i128:128-n32:64-S128-Fn32".into(),
        arch: "aarch64".into(),
        options: TargetOptions {
            features: "+v8a".into(),
            max_atomic_width: Some(128),
            stack_probes: StackProbeType::Inline,
            ..base::vxworks::opts()
        },
    }
}
