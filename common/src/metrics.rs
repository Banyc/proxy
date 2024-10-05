use std::time::Duration;

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
