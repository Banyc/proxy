use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use hdv_derive::HdvSerde;
use monitor_table::row::LiteralValue;
use primitive::ops::unit::{HumanBytes, HumanDuration};

pub fn display_value(header: &str, value: Option<LiteralValue>) -> String {
    let Some(v) = value else {
        return String::new();
    };
    match header {
        "dur" | "duration" => {
            let duration = match v {
                LiteralValue::Int(duration) => duration as u64,
                LiteralValue::UInt(duration) => duration,
                LiteralValue::Float(duration) => duration as u64,
                _ => return v.to_string(),
            };
            let duration = Duration::from_millis(duration);
            let duration = HumanDuration(duration);
            format!("{duration:.1}")
        }
        "bytes" | "up.bytes" | "dn.bytes" => {
            let bytes = match v {
                LiteralValue::Int(bytes) => bytes as u64,
                LiteralValue::UInt(bytes) => bytes,
                LiteralValue::Float(bytes) => bytes as u64,
                _ => return v.to_string(),
            };
            let bytes = HumanBytes(bytes);
            format!("{bytes:.1}")
        }
        "thruput" | "up.thruput" | "dn.thruput" => {
            let thruput = match v {
                LiteralValue::Int(thruput) => thruput as u64,
                LiteralValue::UInt(thruput) => thruput,
                LiteralValue::Float(thruput) => thruput as u64,
                _ => return v.to_string(),
            };
            let thruput = HumanBytes(thruput);
            format!("{thruput:.1}/s")
        }
        _ => v.to_string(),
    }
}

#[derive(Debug, HdvSerde)]
pub struct GaugeView {
    pub thruput: f64,
    pub bytes: u64,
}
impl GaugeView {
    pub fn from_gauge_handle(g: &Mutex<tokio_throughput::GaugeHandle>, now: Instant) -> Self {
        let mut g = g.lock().unwrap();
        g.update(now);
        Self {
            thruput: g.thruput(),
            bytes: g.total_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every header class must pick its own formatter, every value kind must
    /// be converted (or fall through unchanged), and an absent value must
    /// render as the empty string. The table drives one row per arm so a
    /// dropped or reordered arm fails here.
    #[test]
    fn each_header_class_formats_its_own_value_kind() {
        let dur = |v| display_value("dur", Some(v));
        let bytes = |v| display_value("bytes", Some(v));
        let thruput = |v| display_value("thruput", Some(v));

        let cases: Vec<(&str, String)> = vec![
            // "dur" | "duration": integer, unsigned and float milliseconds.
            (
                "duration is the alias",
                display_value("duration", Some(LiteralValue::UInt(1500))),
            ),
            ("int milliseconds", dur(LiteralValue::Int(1500))),
            (
                "float milliseconds truncate",
                dur(LiteralValue::Float(1500.9)),
            ),
            (
                "a non-numeric duration falls through",
                dur(LiteralValue::Bool(true)),
            ),
            // "bytes" | "up.bytes" | "dn.bytes": every alias and value kind.
            (
                "up.bytes alias",
                display_value("up.bytes", Some(LiteralValue::Int(2048))),
            ),
            (
                "dn.bytes alias",
                display_value("dn.bytes", Some(LiteralValue::Float(2048.0))),
            ),
            ("uint bytes", bytes(LiteralValue::UInt(2048))),
            (
                "a non-numeric byte count falls through",
                bytes(LiteralValue::String("n/a".into())),
            ),
            // "thruput" | "up.thruput" | "dn.thruput": the byte form plus "/s".
            (
                "up.thruput alias",
                display_value("up.thruput", Some(LiteralValue::Int(2048))),
            ),
            (
                "dn.thruput alias",
                display_value("dn.thruput", Some(LiteralValue::Float(2048.0))),
            ),
            ("uint thruput", thruput(LiteralValue::UInt(2048))),
            (
                "a non-numeric thruput falls through",
                thruput(LiteralValue::Bool(false)),
            ),
            // Unknown header: the value's own rendering.
            (
                "unknown header",
                display_value("unknown", Some(LiteralValue::UInt(5))),
            ),
        ];

        let expected = [
            "1.5 s", "1.5 s", "1.5 s", "true", "2.0 KB", "2.0 KB", "2.0 KB", "n/a", "2.0 KB/s",
            "2.0 KB/s", "2.0 KB/s", "false", "5",
        ];
        assert_eq!(cases.len(), expected.len());
        for ((name, got), want) in cases.iter().zip(expected) {
            assert_eq!(got, want, "{name}");
        }

        assert_eq!(display_value("dur", None), "", "an absent value is empty");
        assert_eq!(display_value("bytes", None), "");
    }
}
