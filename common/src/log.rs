use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use file_rotating_log::{
    LogWriter,
    rotator::{LogRotator, RotationPolicy, spawn_flushers},
    time_past::{DailyContains, TimePast},
};
use hdv::{
    io::text::{HdvTextWriter, HdvTextWriterOptions},
    serde::{HdvScheme, HdvSerialize},
};
use hdv_derive::HdvSerde;

const FLUSH_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub struct HdvLogger<T> {
    rotator: Arc<Mutex<LogRotator<HdvLogWriter<T>>>>,
}
impl<T> HdvLogger<T>
where
    T: HdvScheme + HdvSerialize + Sync + Send + 'static,
{
    pub fn new(output_dir: PathBuf) -> Self {
        let time_past = TimePast::new(Arc::new(DailyContains));
        let rotation = RotationPolicy {
            max_records: Some(NonZeroUsize::new(1024 * 64).unwrap()),
            time: Some(time_past),
            max_epochs: 4,
        };
        let rotator = LogRotator::new(output_dir, rotation);
        let rotator = Arc::new(Mutex::new(rotator));
        spawn_flushers(vec![Arc::clone(&rotator)], FLUSH_INTERVAL);
        Self { rotator }
    }

    pub fn write(&self, record: &T) {
        let mut rotator = self.rotator.lock().unwrap();
        if rotator.writer().write_or_warn(record) {
            rotator.incr_record_count();
        }
    }

    pub fn flush(&self) {
        self.rotator.lock().unwrap().flush();
    }
}

#[derive(Debug)]
struct HdvLogWriter<T, W = std::fs::File> {
    writer: HdvTextWriter<W, T>,
    warned: bool,
}
impl<T, W> HdvLogWriter<T, W>
where
    W: std::io::Write,
{
    fn write_or_warn(&mut self, record: &T) -> bool
    where
        T: HdvScheme + HdvSerialize,
    {
        match self.writer.write(record) {
            Ok(()) => {
                self.warned = false;
                true
            }
            Err(e) => {
                if !std::mem::replace(&mut self.warned, true) {
                    tracing::warn!(?e, "Failed to write a log record");
                }
                false
            }
        }
    }

    fn flush_or_warn(&mut self)
    where
        T: HdvScheme + HdvSerialize,
    {
        if let Err(e) = self.writer.flush()
            && !std::mem::replace(&mut self.warned, true)
        {
            tracing::warn!(?e, "Failed to flush the log file");
        }
    }
}
impl<T> LogWriter for HdvLogWriter<T>
where
    T: HdvScheme + HdvSerialize,
{
    fn flush(&mut self) {
        self.flush_or_warn();
    }

    fn open(path: impl AsRef<Path>) -> Self {
        let file = std::fs::File::options()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .expect("Cannot create a log file");
        let options = HdvTextWriterOptions {
            is_csv_header: true,
        };
        let writer = HdvTextWriter::new(file, options);
        Self {
            writer,
            warned: false,
        }
    }

    fn file_extension() -> &'static str {
        "csv"
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Timing {
    pub start: (Instant, SystemTime),
    pub end: Instant,
}
impl Timing {
    pub fn duration(&self) -> Duration {
        self.end - self.start.0
    }
}

#[derive(Debug, Clone, HdvSerde)]
pub struct TimingHdv {
    pub start_ms: u64,
    pub duration_ms: u64,
}
impl From<&Timing> for TimingHdv {
    fn from(value: &Timing) -> Self {
        let start_ms = value
            .start
            .1
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let duration_ms = value.duration().as_millis() as u64;
        Self {
            start_ms,
            duration_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Debug, HdvSerde)]
    struct Record {
        pub n: u64,
    }
    struct NoRoomLeft;
    impl std::io::Write for NoRoomLeft {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("no space left on device"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("no space left on device"))
        }
    }
    fn no_room_left() -> HdvLogWriter<Record, NoRoomLeft> {
        let options = HdvTextWriterOptions {
            is_csv_header: true,
        };
        HdvLogWriter {
            writer: HdvTextWriter::new(NoRoomLeft, options),
            warned: false,
        }
    }
    #[test]
    fn a_log_file_that_cannot_be_written_does_not_panic_the_caller() {
        let mut writer = no_room_left();
        assert!(!writer.write_or_warn(&Record { n: 1 }));
        writer.flush_or_warn();
    }
}
