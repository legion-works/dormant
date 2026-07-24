//! Shared MQTT utilities used by the sensor plane, the doctor probes, and
//! the daemon-side hook engine.
//!
//! These live in `dormant-core` because three callers share them
//! (`dormant-sensors`, `dormant-doctor`, `dormantd::hooks`); keeping a single
//! implementation avoids the subtle behavioural drift that creeps in when
//! each crate rolls its own parser (a prior divergence here caused the
//! hook engine to reject host-only URLs the other two accepted).

/// Parse a broker URL into `(host, port)`.
///
/// Accepts `tcp://`, `mqtt://`, or a bare `host:port`. When the URL has no
/// explicit port (or has an unparsable one), falls back to the MQTT default
/// `1883`. Best-effort — callers should validate the URL at config-load
/// time if they need to distinguish "missing port" from "default port".
#[must_use]
pub fn parse_broker_url(url: &str) -> (&str, u16) {
    let rest = url
        .strip_prefix("tcp://")
        .or_else(|| url.strip_prefix("mqtt://"))
        .unwrap_or(url);
    if let Some((host, port_str)) = rest.rsplit_once(':')
        && let Ok(port) = port_str.parse::<u16>()
    {
        return (host, port);
    }
    (rest, 1883)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_broker_url_tcp_prefix() {
        let (host, port) = parse_broker_url("tcp://mqtt.local:1883");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_mqtt_prefix() {
        let (host, port) = parse_broker_url("mqtt://broker.example:8883");
        assert_eq!(host, "broker.example");
        assert_eq!(port, 8883);
    }

    #[test]
    fn parse_broker_url_plain() {
        let (host, port) = parse_broker_url("127.0.0.1:1883");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_default_port_for_host_only() {
        let (host, port) = parse_broker_url("mqtt.local");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_default_port_for_tcp_prefix_no_port() {
        let (host, port) = parse_broker_url("tcp://mqtt.local");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }
}
