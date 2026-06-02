/// Embedded seed JSON (compile-time inclusion from repo root).
pub const SEED_JSON: &str = include_str!("../../model_seed.json");

/// Parse the embedded seed JSON into ModelMappingRow structs.
/// Panics on malformed JSON (intentional — breaks the build if JSON is bad).
pub fn parse_seed() -> Vec<crate::db::model_mappings::ModelMappingRow> {
    serde_json::from_str(SEED_JSON).expect("model_seed.json is malformed")
}
