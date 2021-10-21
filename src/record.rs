#![allow(dead_code)]
use crate::room::Room;
use mediasoup::prelude::*;
use mediasoup::rtp_parameters::{RtpCodecCapabilityFinalized, RtpCodecParameters};
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr};
use std::num::{NonZeroU32, NonZeroU8};
use std::ops::{Deref, DerefMut};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Child;
use tokio::time::{sleep, Duration};

#[derive(Debug)]
pub struct RecordInfo {
    pub remote_port: u16,
    pub rtp_capabilities: RtpCapabilities,
    pub rtp_parameters: RtpParameters,
    pub media_kind: MediaKind,
}

pub async fn publish_producer_rtp_stream(
    room: Room,
    producer: Producer,
    remote_port: u16,
) -> anyhow::Result<(RecordInfo, Consumer)> {
    let transport = room
        .router()
        .create_plain_transport(PlainTransportOptions::new(TransportListenIp {
            ip: [192, 168, 0, 108].into(),
            announced_ip: None,
        }))
        .await?;

    // let remote_port = get_port::udp::UdpPort::in_range(
    //     "127.0.0.1",
    //     Range {
    //         min: 30000,
    //         max: 40000,
    //     },
    // )
    // .expect("no avaliable port");

    transport
        .connect(PlainTransportRemoteParameters {
            ip: Some(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1))),
            port: Some(remote_port),
            rtcp_port: None,
            srtp_parameters: None,
        })
        .await?;

    log::info!(
        "transport id: {}, tuple: {:#?}",
        transport.id(),
        transport.tuple()
    );

    let mut rtp_capabilities = RtpCapabilities::default();

    for rccf in &room.router().rtp_capabilities().codecs {
        match producer.kind() {
            MediaKind::Audio => {
                if let RtpCodecCapabilityFinalized::Audio {
                    mime_type,
                    preferred_payload_type,
                    clock_rate,
                    channels,
                    parameters,
                    rtcp_feedback,
                } = rccf.clone()
                {
                    rtp_capabilities.codecs.push(RtpCodecCapability::Audio {
                        mime_type,
                        preferred_payload_type: Some(preferred_payload_type),
                        clock_rate,
                        channels,
                        parameters,
                        rtcp_feedback,
                    });
                }
            }
            MediaKind::Video => {
                if let RtpCodecCapabilityFinalized::Video {
                    mime_type,
                    preferred_payload_type,
                    clock_rate,
                    parameters,
                    rtcp_feedback,
                } = rccf.clone()
                {
                    rtp_capabilities.codecs.push(RtpCodecCapability::Video {
                        mime_type,
                        preferred_payload_type: Some(preferred_payload_type),
                        clock_rate,
                        parameters,
                        rtcp_feedback,
                    });
                }
            }
        }
    }

    let mut consumer_options = ConsumerOptions::new(producer.id(), rtp_capabilities.clone());
    consumer_options.paused = true;

    let consumer = tokio::task::spawn_blocking(move || {
        futures::executor::block_on(async move { transport.consume(consumer_options).await })
    })
    .await??;
    let rtp_parameters = consumer.rtp_parameters().clone();

    Ok((
        RecordInfo {
            remote_port,
            rtp_capabilities,
            rtp_parameters,
            media_kind: producer.kind(),
        },
        consumer,
    ))
}

#[derive(Debug)]
pub struct CodecInfo {
    payload_type: u8,
    codec_name: String,
    clock_rate: NonZeroU32,
    channels: Option<NonZeroU8>,
}

pub fn serializable_to_string<T: Serialize>(t: &T) -> String {
    let s = serde_json::to_string(t).expect("failed to serialize");
    s[1..s.len() - 1].to_string()
}

fn get_codec_info_from_rtp_parameters(info: &RecordInfo) -> Option<CodecInfo> {
    println!("{:?}", info);
    info.rtp_parameters.codecs.get(0).map(|codec| match codec {
        RtpCodecParameters::Audio {
            payload_type,
            mime_type,
            clock_rate,
            channels,
            ..
        } => CodecInfo {
            payload_type: *payload_type,
            codec_name: serializable_to_string(&mime_type).replace("audio/", ""),
            clock_rate: *clock_rate,
            channels: Some(*channels),
        },
        RtpCodecParameters::Video {
            payload_type,
            mime_type,
            clock_rate,
            ..
        } => CodecInfo {
            payload_type: *payload_type,
            codec_name: serializable_to_string(&mime_type).replace("video/", ""),
            clock_rate: *clock_rate,
            channels: None,
        },
    })
}

fn create_sdp_text(video_info: RecordInfo, audio_info: RecordInfo) -> String {
    let video_codec_info =
        get_codec_info_from_rtp_parameters(&video_info).expect("no video codec info");
    let audio_codec_info =
        get_codec_info_from_rtp_parameters(&audio_info).expect("no audio codec info");

    log::info!("{:?}", video_codec_info);
    log::info!("{:?}", audio_codec_info);

    format!(
        "v=0
  o=- 0 0 IN IP4 127.0.0.1
  s=FFmpeg
  c=IN IP4 127.0.0.1
  t=0 0
  m=video {} RTP/AVP {}
  a=rtpmap:{} {}/{}
  a=sendonly
  m=audio {} RTP/AVP {}
  a=rtpmap:{} {}/{}/{}
  a=sendonly
  ",
        video_info.remote_port,
        video_codec_info.payload_type,
        video_codec_info.payload_type,
        video_codec_info.codec_name,
        video_codec_info.clock_rate,
        audio_info.remote_port,
        audio_codec_info.payload_type,
        audio_codec_info.payload_type,
        audio_codec_info.codec_name,
        audio_codec_info.clock_rate,
        audio_codec_info.channels.expect("channels is none")
    )
}

pub struct Recorder(Child, Vec<Consumer>);

impl Recorder {
    const VIDEO_ARGS: &'static [&'static str; 4] = &["-map", "0:v:0", "-c:v", "copy"];
    const AUDIO_ARGS: &'static [&'static str; 6] =
        &["-map", "0:a:0", "-strict", "-2", "-c:a", "copy"];

    pub async fn start_record(
        vi: RecordInfo,
        ai: RecordInfo,
        consumers: Vec<Consumer>,
    ) -> std::io::Result<Self> {
        let sdp = create_sdp_text(vi, ai);
        log::info!("sdp: {}", sdp);

        let mut child = tokio::process::Command::new("ffmpeg")
            .stdin(Stdio::piped())
            .args(&[
                "-loglevel",
                "debug",
                "-protocol_whitelist",
                "pipe,udp,rtp",
                "-fflags",
                "+genpts",
                "-f",
                "sdp",
                "-i",
                "pipe:0",
            ])
            .args(Self::VIDEO_ARGS)
            .args(Self::AUDIO_ARGS)
            .args(&["-flags", "+global_header", "./fuckme.webm"])
            .spawn()?;

        let mut stdin = child.stdin.take().expect("no stdin");
        stdin.write_all(sdp.as_bytes()).await?;
        stdin.write_u8(0).await?;
        stdin.flush().await?;

        let cs = consumers.clone();
        tokio::spawn(async move {
            sleep(Duration::from_secs(1)).await;

            for c in cs {
                if let Err(e) = c.resume().await {
                    log::error!("resume consumer {} error: {}", c.id(), e);
                }
            }
        });

        Ok(Self(child, consumers))
    }

    pub async fn kill(mut self) -> std::io::Result<()> {
        self.0.kill().await
    }
}

impl Deref for Recorder {
    type Target = Child;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Recorder {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
