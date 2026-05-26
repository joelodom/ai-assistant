//! Best-effort geolocation. macOS CoreLocation requires entitlements that
//! a plain cargo binary doesn't get, so we fall back to IP-based lookup via
//! a public service. The user can override in the UI settings panel.

use shared::Geolocation;

const IP_GEO_URL: &str = "https://ipapi.co/json/";

pub fn fetch_ip_geo() -> Option<Geolocation> {
    // Blocking call on purpose — we run it on a background thread when the
    // UI wants it. ureq has no async dep and that's nice here.
    let resp = ureq::get(IP_GEO_URL)
        .timeout(std::time::Duration::from_secs(4))
        .call()
        .ok()?;
    let json: serde_json::Value = resp.into_json().ok()?;
    let lat = json.get("latitude")?.as_f64()?;
    let lon = json.get("longitude")?.as_f64()?;
    let city = json.get("city").and_then(|v| v.as_str()).unwrap_or("");
    let region = json.get("region").and_then(|v| v.as_str()).unwrap_or("");
    let label = if city.is_empty() && region.is_empty() {
        None
    } else if region.is_empty() {
        Some(city.to_string())
    } else {
        Some(format!("{city}, {region}"))
    };
    Some(Geolocation { lat, lon, label })
}
