use std::{fs, path::Path};

use uperf_core::{apps_config_schema, device_config_schema, policy_config_schema};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/schema");
    for (name, schema) in [
        ("device-v2.schema.json", device_config_schema()),
        ("policy-v2.schema.json", policy_config_schema()),
        ("apps-v2.schema.json", apps_config_schema()),
    ] {
        let mut json = serde_json::to_string_pretty(&schema)?;
        json.push('\n');
        fs::write(output.join(name), json)?;
    }
    Ok(())
}
