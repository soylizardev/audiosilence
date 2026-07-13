use plugin_host::bridge::clap::ClapBridge;
use plugin_host::descriptor::PluginFormat;
use plugin_host::instance::{PluginInstance, PluginInstanceId};
use plugin_host::sandbox::SandboxConfig;
use plugin_host::scanner::ScanResult;
use std::path::Path;

pub fn load_plugin(path: &str, sample_rate: f64) -> Result<PluginInstance, String> {
    let plugin_path = Path::new(path);

    let descriptors = match plugin_host::scanner::PluginScanner::scan_file(
        plugin_path,
        PluginFormat::Clap,
    ) {
        ScanResult::Ok(desc) => desc,
        ScanResult::Invalid(msg) => return Err(format!("invalid plugin file: {}", msg)),
        ScanResult::Error(msg) => return Err(format!("error scanning plugin: {}", msg)),
    };

    let descriptor = descriptors
        .first()
        .ok_or_else(|| "no plugins found in the library".to_string())?
        .clone();

    let id = PluginInstanceId(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64,
    );

    let bridge: Box<dyn plugin_host::bridge::PluginBridge> =
        Box::new(ClapBridge::new(&descriptor)?);

    let sandbox_config = SandboxConfig::in_process();

    let mut instance = PluginInstance::new(id, descriptor, bridge, sandbox_config);

    if !instance.activate(sample_rate, 1, 512) {
        return Err("failed to activate plugin".to_string());
    }

    println!(
        "[plugin_host] Plugin loaded: \"{}\" by {} (v{})",
        instance.descriptor.name,
        instance.descriptor.vendor,
        instance.descriptor.version
    );

    Ok(instance)
}
