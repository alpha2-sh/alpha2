#![no_main]

use alpha2_eval::{CompositionPlan, StepOp, plan_composition, plan_source_composition};
use alpha2_model::MAX_INLINED_NODES;
use libfuzzer_sys::fuzz_target;

// Every composition of a loaded project plans (as a clip's composition and as a source) or is
// refused, never panics, and runs in time. Each plan step reads only earlier steps, the output is a
// step, the step count stays within validation's limit, and a source's plan draws no clip.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(project) = alpha2_model::Project::from_json(text) else {
        return;
    };
    let (compositions, sources) = (&project.compositions, &project.sources);
    // The project canvas, which a layout generator's pixels are pixels of.
    let canvas = (project.settings.width, project.settings.height);
    for composition in compositions {
        if let Ok(plan) = plan_composition(composition, compositions, sources, canvas) {
            check(&plan);
        }
        if let Ok(plan) = plan_source_composition(composition, compositions, sources, canvas) {
            check(&plan);
            assert!(!plan.uses_clip(), "{plan:?}");
        }
    }
});

fn check(plan: &CompositionPlan) {
    assert!(plan.output < plan.steps.len(), "{plan:?}");
    assert!(
        plan.steps.len() <= MAX_INLINED_NODES,
        "{}",
        plan.steps.len()
    );
    for (index, step) in plan.steps.iter().enumerate() {
        let inputs = match &step.op {
            StepOp::Clip | StepOp::Media { .. } | StepOp::Generator { .. } => Vec::new(),
            StepOp::Effect { input, .. }
            | StepOp::Transform { input, .. }
            | StepOp::ShapeMask { input, .. } => vec![*input],
            StepOp::Transition { from, to, .. } => vec![*from, *to],
            StepOp::Matte { input, matte, .. } => vec![*input, *matte],
            StepOp::Merge { layers } => layers.iter().map(|layer| layer.input).collect(),
        };
        assert!(inputs.iter().all(|&input| input < index), "{plan:?}");
    }
}
