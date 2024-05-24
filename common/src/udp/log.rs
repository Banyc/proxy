use core::fmt;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use bytesize::ByteSize;
use hdv_derive::HdvSerde;
use once_cell::sync::Lazy;

use crate::log::{HdvLogger, Timing, TimingHdv};

use super::{Flow, FlowHdv};

pub static LOGGER: Lazy<Arc<Mutex<Option<HdvLogger<FlowLogHdv>>>>> =
    Lazy::new(|| Arc::new(Mutex::new(None)));
pub fn init_logger(output_dir: PathBuf) {
    let output_dir = output_dir.join("udp_record");
    let logger = HdvLogger::new(output_dir);
    *LOGGER.lock().unwrap() = Some(logger);
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, HdvSerde)]
pub struct TrafficLog {
    pub bytes: u64,
    pub packets: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FlowLog {
    pub flow: Flow,
    pub timing: Timing,
    pub up: TrafficLog,
    pub dn: TrafficLog,
}

impl fmt::Display for FlowLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let duration = self.timing.duration().as_secs_f64();
        let uplink_speed = self.up.bytes as f64 / duration;
        let downlink_speed = self.dn.bytes as f64 / duration;
        write!(
            f,
            "{:.1}s,up{{{},{},{}/s}},dn{{{},{},{}/s}},up:{},dn:{}",
            duration,
            self.up.packets,
            ByteSize::b(self.up.bytes),
            ByteSize::b(uplink_speed as u64),
            self.dn.packets,
            ByteSize::b(self.dn.bytes),
            ByteSize::b(downlink_speed as u64),
            self.flow.upstream.as_ref().unwrap().0,
            self.flow.downstream.0,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct FlowLogHdv {
    pub flow: FlowHdv,
    pub timing: TimingHdv,
    pub up: TrafficLog,
    pub dn: TrafficLog,
}
impl From<&FlowLog> for FlowLogHdv {
    fn from(value: &FlowLog) -> Self {
        let flow = (&value.flow).into();
        let timing = (&value.timing).into();
        let up = value.up.clone();
        let dn = value.dn.clone();
        Self {
            flow,
            timing,
            up,
            dn,
        }
    }
}
