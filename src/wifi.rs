use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::Result;
use esp_idf_hal::modem::Modem;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{
    esp, esp_netif_dhcp_option_id_t_ESP_NETIF_DOMAIN_NAME_SERVER,
    esp_netif_dhcp_option_mode_t_ESP_NETIF_OP_SET, esp_netif_dhcps_option, esp_netif_dhcps_start,
    esp_netif_dhcps_stop, esp_netif_dns_info_t, esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN,
    esp_netif_set_dns_info, esp_wifi_set_max_tx_power, ESP_IPADDR_TYPE_V4,
};
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod, Configuration, EspWifi, WifiEvent};

/// Name of the network the device broadcasts. Override at build time with
/// `FAIRY_LIGHTS_AP_SSID`.
const AP_SSID: &str = match option_env!("FAIRY_LIGHTS_AP_SSID") {
    Some(ssid) => ssid,
    None => "FairyLights",
};

/// Cap on transmit power, in quarter-dBm (so 44 = 11dBm); the driver's default is 80,
/// the full 20dBm. At full power this board's 3V3 rail sags during transmit bursts and
/// distorts the outgoing frames, so phones can't even see the network. This is a
/// hardware workaround: the real fix is a supply that can source the burst. Raise it
/// only if range is short and the network stays visible.
const MAX_TX_POWER_QUARTER_DBM: i8 = 44;

/// Who is connected to the access point, kept up to date from Wi-Fi events so the main
/// loop can read it without touching the driver.
#[derive(Default)]
pub struct ApStatus {
    clients: AtomicU8,
    blink_pending: AtomicBool,
}

impl ApStatus {
    pub fn client_count(&self) -> u8 {
        self.clients.load(Ordering::Relaxed)
    }

    /// True once per new connection, so the caller can acknowledge it exactly once.
    pub fn take_new_connection(&self) -> bool {
        self.blink_pending.swap(false, Ordering::Relaxed)
    }
}

/// lwIP's `OFFER_DNS` flag (a `#define`, so not in the generated bindings).
const OFFER_DNS: u8 = 0x02;

/// Starts an open (no password) Wi-Fi access point, so
/// the lights can be controlled from any phone that joins the device's own network - no router in range required. The phone gets an
/// address from the device's DHCP server, and the web UI is served from the device's
/// address (logged below; `192.168.71.1` unless the netif defaults change).
///
/// Note that a phone joined to this network has no internet, and many will offer to drop
/// back to mobile data or another network; tell it to stay connected.
pub fn start_access_point(
    modem: Modem<'static>,
    sysloop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
    status: Arc<ApStatus>,
) -> Result<EspWifi<'static>> {
    // The subscription has to outlive this function (and is never unsubscribed), so it's
    // deliberately leaked, like the other long-lived services started from `main`.
    let subscription = sysloop.subscribe::<WifiEvent<'_>, _>(move |event| match event {
        WifiEvent::ApStaConnected(_) => {
            status.clients.fetch_add(1, Ordering::Relaxed);
            status.blink_pending.store(true, Ordering::Relaxed);
        }
        WifiEvent::ApStaDisconnected(_) => {
            let _ = status
                .clients
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        }
        _ => {}
    })?;
    core::mem::forget(subscription);

    let mut wifi = EspWifi::new(modem, sysloop, Some(nvs))?;

    wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
        ssid: AP_SSID.try_into().unwrap(),
        auth_method: AuthMethod::None,
        ..Default::default()
    }))?;

    wifi.start()?;

    // Has to be set after the driver is started, and is reset by `start()`.
    esp!(unsafe { esp_wifi_set_max_tx_power(MAX_TX_POWER_QUARTER_DBM) })?;
    log::info!(
        "Wi-Fi TX power capped at {:.2}dBm",
        MAX_TX_POWER_QUARTER_DBM as f32 / 4.0
    );

    let ip = wifi.ap_netif().get_ip_info()?.ip;

    // The DHCP server doesn't tell clients about a DNS server unless asked to, and
    // without one the phone never queries `dns.rs`, so the captive-portal probes and
    // `fairylights.local` can't resolve. The server has to be stopped to change options.
    let netif = wifi.ap_netif().handle();
    let mut dns_info = esp_netif_dns_info_t::default();
    dns_info.ip.type_ = ESP_IPADDR_TYPE_V4 as _;
    dns_info.ip.u_addr.ip4.addr = u32::from_le_bytes(ip.octets());
    let mut offer_dns = OFFER_DNS;
    unsafe {
        esp!(esp_netif_dhcps_stop(netif))?;
        esp!(esp_netif_set_dns_info(
            netif,
            esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN,
            &mut dns_info
        ))?;
        esp!(esp_netif_dhcps_option(
            netif,
            esp_netif_dhcp_option_mode_t_ESP_NETIF_OP_SET,
            esp_netif_dhcp_option_id_t_ESP_NETIF_DOMAIN_NAME_SERVER,
            &mut offer_dns as *mut u8 as *mut core::ffi::c_void,
            1,
        ))?;
        esp!(esp_netif_dhcps_start(netif))?;
    }
    log::info!("DHCP now offers {ip} as the DNS server");
    log::info!("Access point \"{AP_SSID}\" up - join it and browse to http://{ip}/");

    Ok(wifi)
}
