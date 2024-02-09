use std::time::Duration;

#[derive(Debug, Copy, Clone)]
pub struct SingleRunStatistics {
    pub get_index_rtt: Duration,
    pub get_count_rtt: Duration,
    pub put_count_rtt: Duration,
    pub post_count_rtt: Duration,
}

impl SingleRunStatistics {
    /*
    #[must_use]
    pub fn new(get_index_rtt: Duration, get_count_rtt: Duration, put_count_rtt: Duration, post_count_rtt: Duration) -> Self {
        Self { get_index_rtt, get_count_rtt, put_count_rtt, post_count_rtt }
    }

     */

    #[inline]
    #[must_use]
    pub fn total(&self) -> Duration {
        self.get_index_rtt + self.get_count_rtt + self.put_count_rtt + self.post_count_rtt
    }
}
