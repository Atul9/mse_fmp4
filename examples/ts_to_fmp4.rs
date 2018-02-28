extern crate byteorder;
extern crate clap;
extern crate mpeg2ts;
extern crate mse_fmp4;
#[macro_use]
extern crate trackable;

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use byteorder::{BigEndian, WriteBytesExt};
use clap::{App, Arg};
use mpeg2ts::time::Timestamp;
use mse_fmp4::fmp4::{self, WriteBoxTo, WriteTo};
use mse_fmp4::{aac, avc};
use mse_fmp4::ErrorKind;
use mpeg2ts::pes::{PesPacket, PesPacketReader, ReadPesPacket};
use mpeg2ts::es::{StreamId, StreamType};
use mpeg2ts::ts::{Pid, ReadTsPacket, TsPacket, TsPacketReader, TsPayload};
use trackable::error::ErrorKindExt;

struct AvcStream {
    decoder_configuration_record: Option<avc::AvcDecoderConfigurationRecord>,
    width: Option<usize>,
    height: Option<usize>,
    packets: Vec<PesPacket<Vec<u8>>>,
}
impl AvcStream {
    fn duration(&self) -> u64 {
        // TODO:
        let start = self.packets.first().unwrap().header.pts.unwrap().as_u64();
        let end = self.packets.last().unwrap().header.pts.unwrap().as_u64();
        end - start
    }
}

struct AacStream {
    packets: Vec<PesPacket<Vec<u8>>>,
}
impl AacStream {
    fn duration(&self) -> u32 {
        let mut duration = 0;
        for p in &self.packets {
            let header = track_try_unwrap!(aac::AdtsHeader::read_from(&p.data[..]));
            duration += header.duration();
        }
        duration
    }
    fn timescale(&self) -> u32 {
        // TDOO:
        let header = track_try_unwrap!(aac::AdtsHeader::read_from(&self.packets[0].data[..]));
        header.timescale()
    }
    fn channels(&self) -> u16 {
        // TDOO:
        let header = track_try_unwrap!(aac::AdtsHeader::read_from(&self.packets[0].data[..]));
        header.channel_configuration as u16
    }
    fn adts_header(&self) -> aac::AdtsHeader {
        // TDOO:
        track_try_unwrap!(aac::AdtsHeader::read_from(&self.packets[0].data[..]))
    }
}

fn read_avc_aac_stream() -> mse_fmp4::Result<(AvcStream, AacStream)> {
    let mut avc_stream = AvcStream {
        decoder_configuration_record: None,
        height: None,
        width: None,
        packets: Vec::new(),
    };

    let mut aac_stream = AacStream {
        packets: Vec::new(),
    };

    let mut is_first_video = true;
    let reader = MyTsPacketReader {
        inner: TsPacketReader::new(std::io::stdin()),
        pid_to_stream_type: HashMap::new(),
        stream_id_to_pid: HashMap::new(),
    };
    let mut reader = PesPacketReader::new(reader);
    while let Some(pes) = track!(
        reader
            .read_pes_packet()
            .map_err(|e| ErrorKind::Other.takes_over(e))
    )? {
        let stream_type = track_assert_some!(
            reader
                .ts_packet_reader()
                .get_stream_type(pes.header.stream_id),
            ErrorKind::InvalidInput
        );
        if !pes.header.stream_id.is_video() {
            track_assert!(pes.header.stream_id.is_audio(), ErrorKind::InvalidInput);
            track_assert_eq!(stream_type, StreamType::AdtsAac, ErrorKind::Unsupported);
            // {
            //     let mut reader = &pes.data[..];
            //     while !reader.is_empty() {
            //         let header = track!(aac::AdtsHeader::read_from(&mut reader))?;
            //         println!("@ {:?}", header);
            //         reader = &reader[header.frame_len_exclude_header() as usize..];
            //     }
            // }
            aac_stream.packets.push(pes);
            continue;
        }

        if is_first_video {
            is_first_video = false;
            track_assert_eq!(stream_type, StreamType::H264, ErrorKind::Unsupported);

            let mut sps = None;
            let mut pps = None;
            let mut sps_info = None;
            let nal_units = track!(avc::ByteStreamFormatNalUnits::new(&pes.data))?;
            for nal_unit in nal_units {
                let nal = track!(avc::NalUnit::read_from(nal_unit))?;
                match nal.nal_unit_type {
                    avc::NalUnitType::SequenceParameterSet => {
                        sps_info = Some(track!(avc::SequenceParameterSet::read_from(
                            &nal_unit[1..]
                        ))?);
                        sps = Some(nal_unit.to_owned());
                    }
                    avc::NalUnitType::PictureParameterSet => {
                        pps = Some(nal_unit.to_owned());
                    }
                    _ => {}
                }
            }

            let sps_info = track_assert_some!(sps_info, ErrorKind::InvalidInput);
            let sps = track_assert_some!(sps, ErrorKind::InvalidInput);
            let pps = track_assert_some!(pps, ErrorKind::InvalidInput);
            avc_stream.decoder_configuration_record = Some(avc::AvcDecoderConfigurationRecord {
                profile_idc: sps_info.profile_idc,
                constraint_set_flag: sps_info.constraint_set_flag,
                level_idc: sps_info.level_idc,
                sequence_parameter_set: sps,
                picture_parameter_set: pps,
            });
            avc_stream.width = Some(sps_info.width());
            avc_stream.height = Some(sps_info.height());
        }
        avc_stream.packets.push(pes);
    }

    Ok((avc_stream, aac_stream))
}

fn main() {
    let matches = App::new("ts_to_fmp4")
        .arg(
            Arg::with_name("OUTPUT_FILE_PREFIX")
                .long("output-file-prefix")
                .takes_value(true)
                .default_value("movie"),
        )
        .get_matches();
    let output_file_prefix = matches.value_of("OUTPUT_FILE_PREFIX").unwrap();

    let (avc_stream, aac_stream) = track_try_unwrap!(read_avc_aac_stream());
    let rec = avc_stream.decoder_configuration_record.clone().unwrap();
    writeln!(
        std::io::stderr(),
        "# {:?}: {:02x}{:02x}{:02x}",
        rec,
        rec.profile_idc,
        rec.constraint_set_flag,
        rec.level_idc
    ).unwrap();
    writeln!(
        std::io::stderr(),
        "# {:?}, {:?}",
        avc_stream.width,
        avc_stream.height
    ).unwrap();

    let mut init_seg = fmp4::InitializationSegment::new();
    let video_duration = avc_stream.duration();
    writeln!(
        std::io::stderr(),
        "# DURATION: {}",
        video_duration as f64 / Timestamp::RESOLUTION as f64,
    ).unwrap();
    init_seg.moov_box.mvhd_box.timescale = Timestamp::RESOLUTION as u32;
    init_seg.moov_box.mvhd_box.duration = video_duration;

    // video track
    let mut t = fmp4::TrackBox::new(true);
    t.tkhd_box.width = (avc_stream.width.unwrap() as u32) << 16;
    t.tkhd_box.height = (avc_stream.height.unwrap() as u32) << 16;
    t.tkhd_box.duration = video_duration;
    t.mdia_box.mdhd_box.timescale = Timestamp::RESOLUTION as u32;
    t.mdia_box.mdhd_box.duration = video_duration;

    let avc_sample_entry = fmp4::AvcSampleEntry {
        width: avc_stream.width.unwrap() as u16,
        height: avc_stream.height.unwrap() as u16,
        avcc_box: fmp4::AvcConfigurationBox {
            config: avc_stream.decoder_configuration_record.clone().unwrap(),
        },
    };
    t.mdia_box
        .minf_box
        .stbl_box
        .stsd_box
        .sample_entries
        .push(track_try_unwrap!(avc_sample_entry.to_sample_entry()));
    init_seg.moov_box.trak_boxes.push(t);

    // audio track
    let audio_duration = aac_stream.duration();
    let mut t = fmp4::TrackBox::new(false);
    t.tkhd_box.duration = u64::from(audio_duration);
    t.mdia_box.mdhd_box.timescale = aac_stream.timescale();
    t.mdia_box.mdhd_box.duration = u64::from(audio_duration);

    let adts_header = aac_stream.adts_header();
    let aac_sample_entry = fmp4::Mpeg4AudioSampleEntry::new(fmp4::AudioSampleEntry {
        channels: aac_stream.channels(),
        sample_rate: aac_stream.timescale() as u16,
        esds_box: fmp4::Mpeg4EsDescriptorBox {
            profile: adts_header.profile,
            frequency: adts_header.sampling_frequency,
            channel_configuration: adts_header.channel_configuration,
        },
    });
    t.mdia_box
        .minf_box
        .stbl_box
        .stsd_box
        .sample_entries
        .push(track_try_unwrap!(aac_sample_entry.to_sample_entry()));
    init_seg.moov_box.trak_boxes.push(t);

    //
    init_seg.moov_box.mvex_box.mehd_box.fragment_duration = video_duration;
    init_seg
        .moov_box
        .mvex_box
        .trex_boxes
        .push(fmp4::TrackExtendsBox::new(1));
    init_seg
        .moov_box
        .mvex_box
        .trex_boxes
        .push(fmp4::TrackExtendsBox::new(2));
    {
        let out = track_try_unwrap!(
            File::create(format!("{}-init.mp4", output_file_prefix))
                .map_err(|e| ErrorKind::Other.cause(e))
        );
        track_try_unwrap!(init_seg.write_to(out))
    }

    let mut media_seg = fmp4::MediaSegment::new();
    let mut video_mdat = fmp4::MediaDataBox { data: Vec::new() };
    let mut audio_mdat = fmp4::MediaDataBox { data: Vec::new() };

    // video traf
    let mut traf = fmp4::TrackFragmentBox::new(1);
    // traf.tfhd_box.default_sample_duration =
    //     Some(video_duration as u32 / avc_stream.packets.len() as u32); // TODO
    traf.tfhd_box.default_sample_flags = Some(
        fmp4::SampleFlags {
            // TODO:
            is_leading: 0,
            sample_depends_on: 1,
            sample_is_depdended_on: 0,
            sample_has_redundancy: 0,
            sample_padding_value: 0,
            sample_is_non_sync_sample: true,
            sample_degradation_priority: 0,
        }.to_u32(),
    );
    traf.trun_box.data_offset = Some(0); // dummy
    traf.trun_box.first_sample_flags = Some(
        fmp4::SampleFlags {
            // TODO:
            is_leading: 0,
            sample_depends_on: 2,
            sample_is_depdended_on: 0,
            sample_has_redundancy: 0,
            sample_padding_value: 0,
            sample_is_non_sync_sample: false,
            sample_degradation_priority: 0,
        }.to_u32(),
    );
    let mut prev_pts: Option<Timestamp> = None;
    for pes in &avc_stream.packets {
        let nal_units = track_try_unwrap!(avc::ByteStreamFormatNalUnits::new(&pes.data));
        let mdat_start = video_mdat.data.len();
        for nal_unit in nal_units {
            video_mdat
                .data
                .write_u32::<BigEndian>(nal_unit.len() as u32)
                .unwrap();
            video_mdat.data.write_all(nal_unit).unwrap();
        }
        let sample_size = (video_mdat.data.len() - mdat_start) as u32;

        let pts = pes.header.pts.unwrap();
        let dts = pes.header.dts.unwrap();
        let sample_composition_time_offset = (pts.as_u64() as i64 - dts.as_u64() as i64) as i32;
        let duration = // TODO
                if let Some(prev) = prev_pts {
                    // TODO: edts
                    // TODO: handle reorder
                     (pts.as_u64() - prev.as_u64()) as u32
                } else {
                    sample_composition_time_offset as u32
                };
        let entry = fmp4::TrunEntry {
            sample_duration: Some(duration),
            sample_size: Some(sample_size),
            sample_flags: None,
            sample_composition_time_offset: Some(sample_composition_time_offset),
        };
        traf.trun_box.entries.push(entry);
        prev_pts = Some(pts);
    }
    media_seg.moof_box.traf_boxes.push(traf);

    // audio traf
    let mut traf = fmp4::TrackFragmentBox::new(2);
    traf.tfhd_box.default_sample_duration = Some(1024); // TODO
    traf.trun_box.data_offset = Some(0); // dummy
    for pes in &aac_stream.packets {
        let mut reader = &pes.data[..];
        while !reader.is_empty() {
            let adts_header = track_try_unwrap!(aac::AdtsHeader::read_from(&mut reader));
            let sample_size = adts_header.frame_len_exclude_header();
            let entry = fmp4::TrunEntry {
                sample_duration: None,
                sample_size: Some(u32::from(sample_size)),
                sample_flags: None,
                sample_composition_time_offset: None,
            };
            traf.trun_box.entries.push(entry);

            audio_mdat
                .data
                .extend_from_slice(&reader[..sample_size as usize]);
            reader = &reader[sample_size as usize..];
        }
    }
    media_seg.moof_box.traf_boxes.push(traf);

    //
    let mut counter = fmp4::WriteBytesCounter::new();
    media_seg.moof_box.write_box_to(&mut counter).unwrap();
    media_seg.moof_box.traf_boxes[0].trun_box.data_offset = Some(counter.count() as i32 + 8);

    media_seg.mdat_boxes.push(video_mdat);

    media_seg.mdat_boxes[0].write_box_to(&mut counter).unwrap();
    media_seg.moof_box.traf_boxes[1].trun_box.data_offset = Some(counter.count() as i32 + 8);

    media_seg.mdat_boxes.push(audio_mdat);
    {
        let out = track_try_unwrap!(
            File::create(format!("{}.m4s", output_file_prefix))
                .map_err(|e| ErrorKind::Other.cause(e))
        );
        track_try_unwrap!(media_seg.write_to(out))
    }
}

struct MyTsPacketReader<R> {
    inner: TsPacketReader<R>,
    pid_to_stream_type: HashMap<Pid, StreamType>,
    stream_id_to_pid: HashMap<StreamId, Pid>,
}
impl<R> MyTsPacketReader<R> {
    fn get_stream_type(&self, stream_id: StreamId) -> Option<StreamType> {
        self.stream_id_to_pid
            .get(&stream_id)
            .and_then(|pid| self.pid_to_stream_type.get(pid))
            .cloned()
    }
}
impl<R: Read> ReadTsPacket for MyTsPacketReader<R> {
    fn read_ts_packet(&mut self) -> mpeg2ts::Result<Option<TsPacket>> {
        if let Some(packet) = track!(self.inner.read_ts_packet())? {
            match packet.payload {
                Some(TsPayload::Pmt(ref pmt)) => for es_info in &pmt.table {
                    self.pid_to_stream_type
                        .insert(es_info.elementary_pid, es_info.stream_type);
                },
                Some(TsPayload::Pes(ref pes)) => {
                    if self.pid_to_stream_type.contains_key(&packet.header.pid) {
                        self.stream_id_to_pid
                            .insert(pes.header.stream_id, packet.header.pid);
                    }
                }
                _ => {}
            }
            Ok(Some(packet))
        } else {
            Ok(None)
        }
    }
}
