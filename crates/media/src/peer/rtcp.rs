//! Interceptor that hands inbound keyframe requests (PLI, FIR) to the application.
//!
//! Inbound RTCP stops at the end of the webrtc interceptor chain unless an interceptor
//! marks it with `Attribute::DeliverToApplication`. The screen publisher needs PLI/FIR
//! to force an IDR frame, so this interceptor, placed last, marks exactly those packets
//! and drops all other inbound RTCP (the earlier interceptors have already acted on it).
//! Adapted from the webrtc `rtcp-processing` example.

use std::collections::VecDeque;
use std::time::Instant;

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;

/// Slot position: application-most, after every built-in interceptor.
pub(crate) const RTCP_FORWARDER_SLOT: usize = 14_000;

/// Whether an RTCP packet asks for a keyframe.
pub(crate) fn is_keyframe_request(packet: &dyn rtc::rtcp::Packet) -> bool {
    let any = packet.as_any();
    any.is::<PictureLossIndication>() || any.is::<FullIntraRequest>()
}

/// See the module documentation.
#[derive(Default)]
pub(crate) struct KeyframeRequestForwarder {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequestForwarder {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(packets) = &msg.message.packet {
            let requests: Vec<Box<dyn rtc::rtcp::Packet>> = packets
                .iter()
                .filter(|p| is_keyframe_request(p.as_ref()))
                .cloned()
                .collect();
            if requests.is_empty() {
                return Ok(());
            }
            msg.message.packet = Packet::Rtcp(requests);
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

impl Interceptor for KeyframeRequestForwarder {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
