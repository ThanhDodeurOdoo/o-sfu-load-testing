pub mod client;
pub mod controller;

mod model;

pub use model::{RESULT_SCHEMA_VERSION, ScenarioResult, ScenarioSpec};

pub const AUTH_KEY: &str = "u6bsUQEWrHdKIuYplirRnbBmLbrKV5PxKG7DtA71mng=";
pub const O_SFU_REVISION: &str = env!("O_SFU_LOCKED_REVISION");
pub const ROOM_ISSUER: &str = "o-sfu-load-testing";
pub const ROOM_KEY: &str = "Y2hhbm5lbC1rZXk=";
