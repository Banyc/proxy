use core::fmt;
use std::{
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
};

use hdv_derive::HdvSerde;
use primitive::ops::unit::HumanBytes;

use crate::{
    log::{HdvLogger, Timing, TimingHdv},
    proxy_runtime::conn::udp::{Flow, FlowHdv},
};

pub static LOGGER: LazyLock<Arc<Mutex<Option<HdvLogger<FlowLogHdv>>>>> =
    LazyLock::new(|| Arc::new(Mutex::new(None)));
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
            "{:.1}s,up{{{},{:.1},{:.1}/s}},dn{{{},{:.1},{:.1}/s}},up:{},dn:{}",
            duration,
            self.up.packets,
            HumanBytes(self.up.bytes),
            HumanBytes(uplink_speed as u64),
            self.dn.packets,
            HumanBytes(self.dn.bytes),
            HumanBytes(downlink_speed as u64),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy_runtime::{
        addr::RouteAddr,
        conn::udp::{DownstreamAddr, UpstreamAddr},
    };
    use std::{
        str::FromStr,
        time::{Instant, SystemTime},
    };

    fn timing() -> Timing {
        let now = Instant::now();
        Timing {
            start: (now, SystemTime::now()),
            end: now,
        }
    }

    /// The `up{}` slot must render the uplink counters and `dn{}` the
    /// downlink counters; swapping them silently mislabels every flow record.
    #[test]
    fn display_reports_upstream_and_downstream_traffic_in_their_own_slots() {
        let log = FlowLog {
            flow: Flow {
                upstream: Some(UpstreamAddr(
                    RouteAddr::from_str("tcp://10.0.0.1:9000").unwrap(),
                )),
                downstream: DownstreamAddr("127.0.0.1:5000".parse().unwrap()),
            },
            timing: timing(),
            up: TrafficLog {
                bytes: 11,
                packets: 1,
            },
            dn: TrafficLog {
                bytes: 22,
                packets: 2,
            },
        };
        let rendered = log.to_string();
        assert!(
            rendered.contains("up{1,"),
            "the up slot must carry the uplink counters: {rendered}"
        );
        assert!(
            rendered.contains("dn{2,"),
            "the dn slot must carry the downlink counters: {rendered}"
        );
    }
}
