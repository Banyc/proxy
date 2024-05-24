use core::fmt;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use bytesize::ByteSize;

use super::Flow;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowLog {
    pub flow: Flow,
    pub start: (Instant, SystemTime),
    pub end: Instant,
    pub bytes_uplink: u64,
    pub bytes_downlink: u64,
    pub packets_uplink: usize,
    pub packets_downlink: usize,
}

impl fmt::Display for FlowLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.end - self.start.0;
        let duration = duration.as_secs_f64();
        let uplink_speed = self.bytes_uplink as f64 / duration;
        let downlink_speed = self.bytes_downlink as f64 / duration;
        write!(
            f,
            "{:.1}s,up{{{},{},{}/s}},dn{{{},{},{}/s}},up:{},dn:{}",
            duration,
            self.packets_uplink,
            ByteSize::b(self.bytes_uplink),
            ByteSize::b(uplink_speed as u64),
            self.packets_downlink,
            ByteSize::b(self.bytes_downlink),
            ByteSize::b(downlink_speed as u64),
            self.flow.upstream.as_ref().unwrap().0,
            self.flow.downstream.0,
        )?;
        Ok(())
    }
}

pub struct FlowRecord<'caller>(pub &'caller FlowLog);
impl<'caller> serde::Serialize for FlowRecord<'caller> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        #[derive(serde::Serialize)]
        struct Record {
            pub upstream_addr: String,
            pub downstream_addr: String,
            pub start_ms: u128,
            pub duration_ms: u128,
            pub bytes_uplink: u64,
            pub bytes_downlink: u64,
            pub packets_uplink: usize,
            pub packets_downlink: usize,
        }
        let duration = self.0.end - self.0.start.0;
        Record {
            upstream_addr: self.0.flow.upstream.as_ref().unwrap().0.to_string(),
            downstream_addr: self.0.flow.downstream.0.to_string(),
            start_ms: self
                .0
                .start
                .1
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
            duration_ms: duration.as_millis(),
            bytes_uplink: self.0.bytes_uplink,
            bytes_downlink: self.0.bytes_downlink,
            packets_uplink: self.0.packets_uplink,
            packets_downlink: self.0.packets_downlink,
        }
        .serialize(serializer)
    }
}
impl<'caller> table_log::LogRecord<'caller> for FlowRecord<'caller> {
    fn table_name(&self) -> &'static str {
        "udp_record"
    }
}
