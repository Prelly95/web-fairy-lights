use anyhow::Result;
use esp_idf_svc::mdns::EspMdns;

/// Resolves to `http://fairylights.local/` for devices joined to the access point, so
/// you don't have to remember the device's IP. Windows needs Bonjour/an mDNS-aware app;
/// most phones, Macs, and Linux resolve `.local` names out of the box.
const HOSTNAME: &str = "fairylights";

/// Starts the mDNS responder. The returned `EspMdns` must be kept alive (not dropped)
/// for as long as the hostname should keep resolving.
pub fn start() -> Result<EspMdns> {
    let mut mdns = EspMdns::take()?;
    mdns.set_hostname(HOSTNAME)?;
    mdns.set_instance_name("Fairy Lights")?;
    mdns.add_service(None, "_http", "_tcp", 80, &[])?;

    log::info!("mDNS responder up - browse to http://{HOSTNAME}.local/");

    Ok(mdns)
}
