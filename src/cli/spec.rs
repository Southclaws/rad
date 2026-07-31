use super::generated::{GlobalArgs, SpecArgs, SpecFormat};
use super::output;
use crate::process::Result;

const OPENCLI: &str = include_str!("../../opencli.yaml");
pub(super) const SCHEMA_JSON: &str = include_str!("../../home/public/rad.schema.json");

pub(super) fn print_spec(globals: &GlobalArgs, args: SpecArgs) -> Result {
    if output::is_json(globals) || args.format == SpecFormat::Json {
        let value: serde_json::Value = serde_yaml::from_str(OPENCLI)?;
        output::print_json(&value)
    } else {
        print!("{OPENCLI}");
        Ok(())
    }
}

pub(super) fn print_schema() {
    print!("{SCHEMA_JSON}");
}
