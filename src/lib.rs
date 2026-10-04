pub mod generate;
pub mod graph;
pub mod models;
pub mod money;
pub mod pipeline;
pub mod publish;
pub mod render;
pub mod server;
pub mod store;
pub mod trends;

pub fn example() -> models::ProjectSpec {
    serde_json::from_str(include_str!("../examples/one-clear-step.json"))
        .expect("bundled example is valid")
}

pub fn preferred_example() -> models::ProjectSpec {
    let mut spec = example();
    spec.voice = render::preferred_voice(&spec.language);
    spec
}
