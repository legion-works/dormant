//! Shared MQTT utilities used by the sensor plane, the doctor probes, and
//! the daemon-side hook engine.
//!
//! These live in `dormant-core` because three callers share them
//! (`dormant-sensors`, `dormant-doctor`, `dormantd::hooks`); keeping a single
//! implementation avoids the subtle behavioural drift that creeps in when
//! each crate rolls its own parser (a prior divergence here caused the
//! hook engine to reject host-only URLs the other two accepted).

/// Failure modes for [`parse_broker_url`]. Variant names are grep-stable
/// literals — never construct them via `format!`.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BrokerUrlError {
    /// The URL parsed but the host segment was empty
    /// (`tcp://`, `tcp://:1883`, `tcp://[]`, ...).
    #[error("broker url {input:?} has empty host")]
    EmptyHost {
        /// The original URL string that failed to parse.
        input: String,
    },
    /// A `:port` suffix was present but did not parse as a `u16`
    /// (e.g. `tcp://host:1883x`). Distinct from "no port at all".
    #[error("broker url {input:?} has malformed port {port:?}")]
    MalformedPort {
        /// The original URL string that failed to parse.
        input: String,
        /// The unparsable port suffix as it appeared in the URL.
        port: String,
    },
}

/// Parse a broker URL into `(host, port)`.
///
/// Accepts `tcp://`, `mqtt://`, or a bare `host:port`. When no explicit
/// port is supplied, falls back to the MQTT default `1883`. The parser is
/// strict about malformed ports and empty hosts — callers receive an
/// error instead of a silently-mishandled URL.
///
/// Recognised shapes (after stripping the optional `tcp://`/`mqtt://`
/// scheme):
///
/// | input                | host       | port  |
/// |----------------------|------------|-------|
/// | `mqtt.local`         | `mqtt.local` | 1883 |
/// | `mqtt.local:1883`    | `mqtt.local` | 1883 |
/// | `tcp://::1`          | `::1`      | 1883  | (bare IPv6, multiple colons)
/// | `tcp://[::1]`        | `::1`      | 1883  | (bracketed IPv6, default port)
/// | `tcp://[::1]:1884`   | `::1`      | 1884  | (bracketed IPv6, explicit port)
/// | `tcp://host:1883x`   | _error_ — `MalformedPort` |
/// | `tcp://`             | _error_ — `EmptyHost`     |
///
/// # Errors
///
/// Returns [`BrokerUrlError::EmptyHost`] when the URL has no resolvable
/// host segment (e.g. `tcp://`, `tcp://:1883`, `tcp://[]`) and
/// [`BrokerUrlError::MalformedPort`] when a `:port` suffix is present but
/// does not parse as a `u16` (e.g. `tcp://host:1883x`, or an unclosed
/// `[` bracket).
pub fn parse_broker_url(url: &str) -> Result<(&str, u16), BrokerUrlError> {
    let rest = url
        .strip_prefix("tcp://")
        .or_else(|| url.strip_prefix("mqtt://"))
        .unwrap_or(url);

    if rest.is_empty() {
        return Err(BrokerUrlError::EmptyHost {
            input: url.to_string(),
        });
    }

    // Bracketed IPv6 literal: [host] or [host]:port.
    if let Some(after_bracket) = rest.strip_prefix('[') {
        let Some((host, after)) = after_bracket.split_once(']') else {
            // `tcp://[::1` — unbalanced bracket. Treat as malformed port.
            return Err(BrokerUrlError::MalformedPort {
                input: url.to_string(),
                port: rest.to_string(),
            });
        };
        if host.is_empty() {
            return Err(BrokerUrlError::EmptyHost {
                input: url.to_string(),
            });
        }
        if let Some((_, port_str)) = after.split_once(':') {
            let port = port_str
                .parse::<u16>()
                .map_err(|_| BrokerUrlError::MalformedPort {
                    input: url.to_string(),
                    port: port_str.to_string(),
                })?;
            Ok((host, port))
        } else {
            // `[host]` with no `:port` suffix — default port.
            Ok((host, 1883))
        }
    } else if rest.matches(':').count() >= 2 {
        // Bare IPv6 literal (multiple colons, no brackets) — default port.
        Ok((rest, 1883))
    } else if let Some((host, port_str)) = rest.rsplit_once(':') {
        // `host:port` form. Empty host (`tcp://:1883`) is an error;
        // non-numeric port is `MalformedPort`.
        if host.is_empty() {
            return Err(BrokerUrlError::EmptyHost {
                input: url.to_string(),
            });
        }
        let port = port_str
            .parse::<u16>()
            .map_err(|_| BrokerUrlError::MalformedPort {
                input: url.to_string(),
                port: port_str.to_string(),
            })?;
        Ok((host, port))
    } else {
        // `host` only — default port.
        Ok((rest, 1883))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_broker_url_tcp_prefix() {
        let (host, port) = parse_broker_url("tcp://mqtt.local:1883").expect("valid URL must parse");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_mqtt_prefix() {
        let (host, port) =
            parse_broker_url("mqtt://broker.example:8883").expect("valid URL must parse");
        assert_eq!(host, "broker.example");
        assert_eq!(port, 8883);
    }

    #[test]
    fn parse_broker_url_plain() {
        let (host, port) = parse_broker_url("127.0.0.1:1883").expect("valid URL must parse");
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_default_port_for_host_only() {
        let (host, port) = parse_broker_url("mqtt.local").expect("host-only URL must parse");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    #[test]
    fn parse_broker_url_default_port_for_tcp_prefix_no_port() {
        let (host, port) = parse_broker_url("tcp://mqtt.local").expect("host-only URL must parse");
        assert_eq!(host, "mqtt.local");
        assert_eq!(port, 1883);
    }

    /// Issue #204: bare IPv6 `tcp://::1` must resolve to host `::1` and the
    /// MQTT default port, not split on the first colon into `("", "1")`.
    #[test]
    fn parse_broker_url_bare_ipv6_default_port() {
        let (host, port) = parse_broker_url("tcp://::1").expect("bare IPv6 must parse");
        assert_eq!(host, "::1");
        assert_eq!(port, 1883);
    }

    /// Issue #204: bracketed IPv6 with explicit port must yield the bare
    /// host (no brackets) and the supplied port.
    #[test]
    fn parse_broker_url_bracketed_ipv6_with_port() {
        let (host, port) = parse_broker_url("tcp://[::1]:1884").expect("bracketed IPv6 must parse");
        assert_eq!(host, "::1");
        assert_eq!(port, 1884);
    }

    /// Issue #204: a port suffix that does not parse as `u16` is a hard
    /// error. Previously fell back to the default port and returned the
    /// whole `host:1883x` string as the host.
    #[test]
    fn parse_broker_url_malformed_port_is_error() {
        let err = parse_broker_url("tcp://host:1883x").expect_err("non-numeric port must error");
        assert!(
            matches!(err, BrokerUrlError::MalformedPort { ref port, .. } if port == "1883x"),
            "expected MalformedPort {{ port: \"1883x\", .. }}, got {err:?}"
        );
    }
}
