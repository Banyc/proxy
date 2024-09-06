use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use file_rotating_log::{
    rotator::{spawn_flushers, LogRotator, RotationPolicy},
    time_past::{DailyContains, TimePast},
    LogWriter,
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
        rotator.writer().writer().write(record).unwrap();
        rotator.incr_record_count();
    }

    pub fn flush(&self) {
        self.rotator.lock().unwrap().flush();
    }
}

#[derive(Debug)]
struct HdvLogWriter<T> {
    writer: HdvTextWriter<std::fs::File, T>,
}
impl<T> HdvLogWriter<T> {
    pub fn writer(&mut self) -> &mut HdvTextWriter<std::fs::File, T> {
        &mut self.writer
    }
}
impl<T> LogWriter for HdvLogWriter<T>
where
    T: HdvScheme + HdvSerialize,
{
    fn flush(&mut self) {
        self.writer.flush().unwrap();
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
        Self { writer }
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
            .unwrap()
            .as_millis() as u64;
        let duration_ms = value.duration().as_millis() as u64;
        Self {
            start_ms,
            duration_ms,
        }
    }
}
