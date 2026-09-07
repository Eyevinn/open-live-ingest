//! The settings form.
//!
//! Everything an operator needs is editable here, so the app never requires a text
//! editor. Fields are held as strings while being typed — a half-typed port range is
//! not a valid range — and converted into a `GatewayConfig` only when saved.

use open_live_gateway_types::config::UplinkMode;
use open_live_gateway_types::GatewayConfig;

pub struct Form {
    pub gateway_name: String,
    pub open_live_url: String,
    pub open_live_key: String,
    pub auth_mode: String,
    pub register: bool,
    pub strom_url: String,
    pub strom_key: String,
    pub cloud_host: String,
    pub uplink_mode: UplinkMode,
    pub public_host: String,
    pub port_range: String,
    pub latency_ms: String,
    pub resolution: String,
    pub framerate: String,
    pub bitrate_kbps: String,
}

impl Form {
    pub fn from_config(cfg: &GatewayConfig) -> Self {
        Self {
            gateway_name: cfg.gateway.name.clone(),
            open_live_url: cfg.open_live.url.clone().unwrap_or_default(),
            open_live_key: cfg.open_live.api_key.clone().unwrap_or_default(),
            auth_mode: cfg.open_live.auth_mode.clone(),
            register: cfg.open_live.register,
            strom_url: cfg.strom.url.clone(),
            strom_key: cfg.strom.api_key.clone().unwrap_or_default(),
            cloud_host: cfg.app.uplink.host.clone().unwrap_or_default(),
            uplink_mode: cfg.app.uplink.mode,
            public_host: cfg.app.uplink.public_host.clone().unwrap_or_default(),
            port_range: cfg.app.uplink.port_range.clone(),
            latency_ms: cfg.app.uplink.latency_ms.to_string(),
            resolution: cfg.app.video_resolution.clone(),
            framerate: cfg.app.video_framerate.clone(),
            bitrate_kbps: cfg.app.video.bitrate_kbps.to_string(),
        }
    }

    /// Folds the form back into a config, keeping anything the form does not expose
    /// (declared inputs, log level, control settings) from the config it came from.
    pub fn to_config(&self, base: &GatewayConfig) -> Result<GatewayConfig, String> {
        let mut cfg = base.clone();

        cfg.gateway.name = self.gateway_name.trim().to_string();
        if cfg.gateway.name.is_empty() {
            return Err("Name cannot be empty — it labels the sources in Studio".to_string());
        }

        cfg.open_live.url = non_empty(&self.open_live_url);
        cfg.open_live.api_key = non_empty(&self.open_live_key);
        cfg.open_live.auth_mode = self.auth_mode.clone();
        cfg.open_live.register = self.register;

        cfg.strom.url = self.strom_url.trim().to_string();
        cfg.strom.api_key = non_empty(&self.strom_key);

        cfg.app.uplink.mode = self.uplink_mode;
        cfg.app.uplink.host = non_empty(&self.cloud_host);
        cfg.app.uplink.public_host = non_empty(&self.public_host);
        cfg.app.uplink.port_range = self.port_range.trim().to_string();
        cfg.app.uplink.latency_ms = self
            .latency_ms
            .trim()
            .parse()
            .map_err(|_| "SRT latency must be a whole number of milliseconds".to_string())?;

        cfg.app.video_resolution = self.resolution.trim().to_string();
        cfg.app.video_framerate = self.framerate.trim().to_string();
        cfg.app.video.bitrate_kbps = self
            .bitrate_kbps
            .trim()
            .parse()
            .map_err(|_| "Bitrate must be a whole number of kbps".to_string())?;

        // Fail here rather than at the next Start: the port range and the uplink
        // direction are only exercised when a device is started, which would put the
        // error a long way from the setting that caused it.
        cfg.app
            .uplink
            .ports()
            .map_err(|e| format!("Port range: {e}"))?;
        if matches!(
            cfg.app.uplink.mode,
            UplinkMode::Listener | UplinkMode::Rendezvous
        ) && cfg.app.uplink.public_host.is_none()
        {
            return Err(
                "This uplink direction has the cloud dial the venue, so it needs this \
                 machine's address as the cloud sees it"
                    .to_string(),
            );
        }

        Ok(cfg)
    }
}

fn non_empty(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> GatewayConfig {
        GatewayConfig {
            gateway: open_live_gateway_types::config::GatewayIdentity {
                id: None,
                name: "Venue".to_string(),
            },
            ..GatewayConfig::default()
        }
    }

    /// A round trip must not silently change anything, or opening the settings and
    /// pressing Save would rewrite an operator's config.
    #[test]
    fn a_round_trip_preserves_the_config() {
        let mut cfg = base();
        cfg.open_live.url = Some("https://open-live.example.com".to_string());
        cfg.open_live.api_key = Some("secret".to_string());
        cfg.app.uplink.port_range = "9500-9600".to_string();
        cfg.app.video.bitrate_kbps = 8000;

        let out = Form::from_config(&cfg).to_config(&cfg).expect("valid");

        assert_eq!(out.open_live.url, cfg.open_live.url);
        assert_eq!(out.open_live.api_key, cfg.open_live.api_key);
        assert_eq!(out.app.uplink.port_range, "9500-9600");
        assert_eq!(out.app.video.bitrate_kbps, 8000);
        assert_eq!(out.strom.url, cfg.strom.url);
    }

    /// Blank means "not set", not an empty string that later fails a URL parse.
    #[test]
    fn blank_fields_become_none() {
        let cfg = base();
        let mut form = Form::from_config(&cfg);
        form.open_live_url = "   ".to_string();
        form.cloud_host = "".to_string();

        let out = form.to_config(&cfg).expect("valid");
        assert!(out.open_live.url.is_none());
        assert!(out.app.uplink.host.is_none(), "blank means discover it");
    }

    #[test]
    fn a_bad_port_range_is_reported_before_saving() {
        let cfg = base();
        let mut form = Form::from_config(&cfg);
        form.port_range = "9100-9000".to_string();
        assert!(form.to_config(&cfg).unwrap_err().contains("Port range"));
    }

    #[test]
    fn a_non_numeric_bitrate_is_reported() {
        let cfg = base();
        let mut form = Form::from_config(&cfg);
        form.bitrate_kbps = "lots".to_string();
        assert!(form.to_config(&cfg).unwrap_err().contains("Bitrate"));
    }

    /// Catch it at Save, not at the next Start, where the cause would be far away.
    #[test]
    fn cloud_dialling_modes_need_a_public_host_at_save_time() {
        let cfg = base();
        let mut form = Form::from_config(&cfg);
        form.uplink_mode = UplinkMode::Listener;
        form.public_host = String::new();
        assert!(form.to_config(&cfg).is_err());

        form.public_host = "venue.example.com".to_string();
        assert!(form.to_config(&cfg).is_ok());
    }

    #[test]
    fn an_empty_name_is_rejected() {
        let cfg = base();
        let mut form = Form::from_config(&cfg);
        form.gateway_name = "  ".to_string();
        assert!(form.to_config(&cfg).unwrap_err().contains("Name"));
    }

    /// Declared inputs belong to the headless daemon and are not shown in the form,
    /// so saving from the app must not delete them.
    #[test]
    fn saving_keeps_settings_the_form_does_not_show() {
        let mut cfg = base();
        cfg.log.level = "debug".to_string();
        cfg.control.token = Some("keep-me".to_string());

        let out = Form::from_config(&cfg).to_config(&cfg).expect("valid");
        assert_eq!(out.log.level, "debug");
        assert_eq!(out.control.token.as_deref(), Some("keep-me"));
    }
}
