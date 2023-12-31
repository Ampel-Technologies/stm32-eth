/// A packet ID.
///
/// This packet ID can be used to obtain information about a specific
/// ethernet frame (either sent or received) from the DMA.
///
#[cfg_attr(
    feature = "ptp",
    doc = "
The main use is obtaining timestamps for frames using [`EthernetDMA::poll_timestamp`](crate::EthernetDMA::poll_timestamp)
"
)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, PartialEq, Clone)]
pub struct PacketId(pub u32);

impl PacketId {
    /// The initial value for an [`Option<PacketId>`]
    pub const INIT: Option<Self> = None;
}

impl From<u32> for PacketId {
    fn from(value: u32) -> Self {
        Self(value)
    }
}

#[cfg(all(feature = "ptp", feature = "smoltcp-phy"))]
impl From<smoltcp::phy::PacketMeta> for PacketId {
    fn from(value: smoltcp::phy::PacketMeta) -> Self {
        Self(value.id)
    }
}

#[cfg(all(feature = "ptp", feature = "smoltcp-phy"))]
impl From<PacketId> for smoltcp::phy::PacketMeta {
    fn from(value: PacketId) -> Self {
        let mut meta = smoltcp::phy::PacketMeta::default();
        meta.id = value.0;
        meta
    }
}
