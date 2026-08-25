//! Validate the WGSL shader without a GPU.
//!
//! wgpu only compiles shaders at pipeline creation, which needs a device, and this
//! sandbox has neither a display nor Vulkan. Running the same front-end that wgpu uses
//! (naga) catches syntax and type errors here rather than on the user's machine.

const SOURCE: &str = include_str!("../src/shader.wgsl");

fn validate() -> naga::valid::ModuleInfo {
    let module = naga::front::wgsl::parse_str(SOURCE).expect("shader must parse as valid WGSL");
    naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::default(),
    )
    .validate(&module)
    .expect("shader must pass naga validation")
}

#[test]
fn shader_parses_and_validates() {
    validate();
}

#[test]
fn shader_exposes_the_entry_points_the_pipeline_asks_for() {
    // gpu.rs names these explicitly; a rename would otherwise fail only at runtime.
    let module = naga::front::wgsl::parse_str(SOURCE).unwrap();
    let names: Vec<&str> = module.entry_points.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"vs"), "missing vertex entry point, got {names:?}");
    assert!(names.contains(&"fs"), "missing fragment entry point, got {names:?}");

    for ep in &module.entry_points {
        match ep.name.as_str() {
            "vs" => assert_eq!(ep.stage, naga::ShaderStage::Vertex),
            "fs" => assert_eq!(ep.stage, naga::ShaderStage::Fragment),
            other => panic!("unexpected entry point {other}"),
        }
    }
}

#[test]
fn shader_bindings_match_the_bind_group_layout() {
    // gpu.rs declares uniform at 0, texture at 1, sampler at 2, all in group 0.
    let module = naga::front::wgsl::parse_str(SOURCE).unwrap();
    let mut found: Vec<(u32, u32)> = module
        .global_variables
        .iter()
        .filter_map(|(_, v)| v.binding.as_ref())
        .map(|b| (b.group, b.binding))
        .collect();
    found.sort_unstable();
    assert_eq!(
        found,
        vec![(0, 0), (0, 1), (0, 2)],
        "shader bindings drifted from the bind group layout in gpu.rs"
    );
}

#[test]
fn vertex_shader_covers_all_six_quad_vertices() {
    // gpu.rs issues `draw(0..6)`. Indexing a 6-element array with a larger count would
    // be an out-of-bounds read, so keep the two in step.
    assert!(
        SOURCE.contains("array<vec2<f32>, 6>"),
        "the corner array should hold exactly 6 vertices to match draw(0..6)"
    );
}
