use crate::{stream::session_table::StreamSessionTable, udp::session_table::UdpSessionTable};

#[derive(Debug, Clone)]
pub struct BothSessionTables<ST> {
    stream: Option<StreamSessionTable<ST>>,
    udp: Option<UdpSessionTable>,
}

impl<ST> BothSessionTables<ST> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            stream: Some(StreamSessionTable::new()),
            udp: Some(UdpSessionTable::new()),
        }
    }

    #[must_use]
    pub fn empty() -> Self {
        Self {
            stream: None,
            udp: None,
        }
    }

    #[must_use]
    pub fn stream(&self) -> Option<&StreamSessionTable<ST>> {
        self.stream.as_ref()
    }

    #[must_use]
    pub fn udp(&self) -> Option<&UdpSessionTable> {
        self.udp.as_ref()
    }
}

impl<ST> Default for BothSessionTables<ST> {
    fn default() -> Self {
        Self::new()
    }
}
