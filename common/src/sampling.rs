use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct TopSample<T> {
    data_points: Vec<DataPoint<T>>,
    max_len: usize,
    fresh: Duration,
}

impl<T> TopSample<T> {
    pub fn new(max_len: usize, fresh: Duration) -> TopSample<T> {
        TopSample {
            data_points: Default::default(),
            max_len,
            fresh,
        }
    }
}

impl<T> TopSample<T>
where
    T: Ord,
{
    pub fn insert(&mut self, value: T) {
        let mut i = 0;
        while i < self.data_points.len() {
            if !self.data_points[i].is_fresh() && self.max_len == self.data_points.len() {
                // Kick out the stale data point for the new one
                self.data_points.swap_remove(i);
                break;
            }
            if self.data_points[i].value < value {
                break;
            }
            i += 1;
        }
        self.data_points.push(DataPoint::new(value, self.fresh));
        self.data_points.sort();
        self.data_points.truncate(self.max_len);
    }

    pub fn get_5_percentile(&self) -> Option<&T> {
        self.get_percentile(0.05)
    }

    pub fn get_percentile(&self, percentile: f64) -> Option<&T> {
        if self.data_points.is_empty() {
            return None;
        }
        let nth = (self.data_points.len() as f64 * percentile) as usize;
        Some(&self.data_points[nth].value)
    }
}

#[derive(Debug, Clone)]
struct DataPoint<T> {
    fresh_until: Instant,
    value: T,
}

impl<T> DataPoint<T> {
    pub fn new(value: T, fresh: Duration) -> DataPoint<T> {
        Self {
            fresh_until: Instant::now() + fresh,
            value,
        }
    }

    pub fn is_fresh(&self) -> bool {
        Instant::now() > self.fresh_until
    }
}

impl<T> PartialEq for DataPoint<T>
where
    T: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T> Eq for DataPoint<T> where T: Eq {}

impl<T> PartialOrd for DataPoint<T>
where
    T: PartialOrd,
{
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        self.value.partial_cmp(&other.value)
    }
}

impl<T> Ord for DataPoint<T>
where
    T: Ord,
{
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.value.cmp(&other.value)
    }
}
