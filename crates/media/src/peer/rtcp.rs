//! Interceptor that hands inbound keyframe requests (PLI, FIR) and receiver reports to
//! the application.
//!
//! Inbound RTCP stops at the end of the webrtc interceptor chain unless an interceptor
//! marks it with `Attribute::DeliverToApplication`. The screen publisher needs PLI/FIR
//! to force an IDR frame, and the voice encoder tunes its FEC to the packet loss in
//! receiver reports, so this interceptor, placed last, marks exactly those packets and
//! drops all other inbound RTCP (the earlier interceptors have already acted on it).
//! Adapted from the webrtc `rtcp-processing` example.

use std::collections::VecDeque;
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtcp::receiver_report::ReceiverReport;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

/// Slot position: application-most, after every built-in interceptor.
pub(crate) const RTCP_FORWARDER_SLOT: usize = 14_000;

/// Whether an RTCP packet asks for a keyframe.
pub(crate) fn is_keyframe_request(packet: &dyn rtc::rtcp::Packet) -> bool {
    let any = packet.as_any();
    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
}

/// `fraction_lost` (0..=255) of the stream `ssrc` if `packet` is a receiver report
/// about it.
pub(crate) fn fraction_lost(packet: &dyn rtc::rtcp::Packet, ssrc: u32) -> Option<u8> {
    let rr = packet.as_any().downcast_ref::<ReceiverReport>()?;
    rr.reports
        .iter()
        .find(|r| r.ssrc == ssrc)
        .map(|r| r.fraction_lost)
}

/// Whether the application wants this inbound RTCP packet.
fn is_forwarded(packet: &dyn rtc::rtcp::Packet) -> bool {
    is_keyframe_request(packet) || packet.as_any().is::<ReceiverReport>()
}

/// See the module documentation.
#[derive(Default)]
pub(crate) struct RtcpFeedbackForwarder {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for RtcpFeedbackForwarder {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let forwarded: Vec<Box<dyn rtc::rtcp::Packet>> = packets
                .iter()
                .filter(|p| is_forwarded(p.as_ref()))
                .cloned()
                .collect();
            if forwarded.is_empty() {
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(forwarded);
            msg.message.add(Attribute::DeliverToApplication);
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for RtcpFeedbackForwarder {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
