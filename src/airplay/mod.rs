//! AirPlay 2 sender, modelled on the realtime-audio path that OwnTone uses
//! with HomePods: transient pair-setup (SRP, PIN 3939), encrypted RTSP control
//! channel, ALAC in RTP payload type 96 encrypted with ChaCha20-Poly1305, and
//! NTP-style timing served by us.

pub mod alac;
pub mod cipher;
pub mod clock;
pub mod rtsp;
pub mod session;
pub mod srp;
pub mod streamer;
pub mod tlv;

/// AirPlay audio is always 44.1 kHz / 16 bit / stereo on this path.
pub const SAMPLE_RATE: u32 = 44_100;
pub const CHANNELS: usize = 2;
/// Frames (stereo samples) per RTP packet.
pub const FRAMES_PER_PACKET: usize = 352;
/// RTP payload type for realtime audio.
pub const RTP_PAYLOAD_TYPE: u8 = 0x60;
