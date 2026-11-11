//! The FFmpeg (libavcodec) decoders behind [`Decoder`] (audio) and
//! [`VideoDecoder`].
//!
//! Everything that touches FFmpeg lives here. `ffmpeg-next` covers most of
//! the API safely; the codec context fields it does not expose (extradata,
//! channel layout, PCM block size) are set through its raw pointer, which
//! is this crate's only `unsafe` code.

#![allow(unsafe_code)]

use ff::codec::Id;
use ff::format::Sample;
use ff::format::sample::Type;
use ffmpeg_next as ff;
use lac_container::wav::SampleEncoding;
use lac_container::{Codec, DecodeSection};
use lac_core::{MediaTime, VideoCodec, VideoStreamInfo};

use crate::picture::{Component, Frame, Matrix, PixelLayout, Plane};

/// An error reported by FFmpeg.
#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    /// FFmpeg returned an error code.
    #[error("{0}")]
    Ffmpeg(#[from] ff::Error),
    /// This FFmpeg build has no decoder for the codec.
    #[error("FFmpeg has no decoder for it")]
    NoDecoder,
    /// The decoder produces samples in a format this crate cannot convert.
    #[error("unsupported sample format {0:?}")]
    SampleFormat(Sample),
    /// The PCM layout of a WAV file has no FFmpeg decoder.
    #[error("unsupported PCM layout ({0:?}, {1} bytes per sample)")]
    PcmLayout(SampleEncoding, usize),
    /// The video decoder produces pictures in a pixel format this crate
    /// cannot read (palettes, bit-packed or hardware formats).
    #[error("unsupported pixel format {0}")]
    PixelFormat(String),
}

/// One opened libavcodec audio decoder.
pub(crate) struct Decoder {
    inner: ff::decoder::Audio,
    frame: ff::frame::Audio,
}

impl Decoder {
    /// Opens a decoder for `section`.
    pub(crate) fn new(section: &DecodeSection) -> Result<Self, FfmpegError> {
        ff::init()?;
        let (id, bits) = codec_id(section)?;
        let codec = ff::decoder::find(id).ok_or(FfmpegError::NoDecoder)?;
        let mut ctx = ff::codec::Context::new_with_codec(codec);
        // SAFETY: `ptr` is the valid, exclusively owned context `ctx` just
        // allocated; it is not opened yet, so setting these fields is
        // allowed. The extradata buffer comes from `av_mallocz` with the
        // padding libavcodec requires and is freed by the context.
        unsafe {
            let ptr = ctx.as_mut_ptr();
            (*ptr).sample_rate = section.sample_rate as i32;
            // Our packets carry no timestamps, but when libavcodec drops
            // leading samples itself (Opus pre-skip, see `delay`) or
            // trailing padding, it shifts the frame's timestamps and logs
            // "Could not update timestamps for skipped samples" for every
            // frame if it lacks a packet time base (libavcodec/decode.c,
            // `discard_samples`). One tick per sample is the natural base;
            // it changes no decoded sample.
            if section.sample_rate > 0 {
                (*ptr).pkt_timebase = ff::ffi::AVRational {
                    num: 1,
                    den: section.sample_rate as i32,
                };
            }
            ff::ffi::av_channel_layout_default(&mut (*ptr).ch_layout, i32::from(section.channels));
            if let Codec::Pcm { block_align, .. } = section.codec {
                (*ptr).block_align = i32::from(block_align);
                (*ptr).bits_per_coded_sample = bits;
            }
            if !section.extradata.is_empty() {
                let len = section.extradata.len();
                let pad = ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize;
                let buf = ff::ffi::av_mallocz(len + pad) as *mut u8;
                if buf.is_null() {
                    return Err(ff::Error::Other {
                        errno: ff::error::ENOMEM,
                    }
                    .into());
                }
                std::ptr::copy_nonoverlapping(section.extradata.as_ptr(), buf, len);
                (*ptr).extradata = buf;
                (*ptr).extradata_size = len as i32;
            }
            if let Codec::Ac3 = section.codec {
                // Seed the dither generator from each frame's content, not
                // once per stream, so that a cut decodes like the source
                // (see lac_container::ac3). Failing to set it only costs
                // that exactness, so the result is ignored. The flag's
                // integer type depends on the bindgen target.
                #[allow(clippy::unnecessary_cast)]
                let _ = ff::ffi::av_opt_set_int(
                    ptr.cast(),
                    c"cons_noisegen".as_ptr(),
                    1,
                    ff::ffi::AV_OPT_SEARCH_CHILDREN as i32,
                );
            }
        }
        let inner = ctx.decoder().audio()?;
        Ok(Decoder {
            inner,
            frame: ff::frame::Audio::empty(),
        })
    }

    /// Leading samples of the codec's output that libavcodec drops by
    /// itself (`AVCodecContext.delay`; for Opus the pre-skip from the
    /// extradata). The first sample this decoder returns is sample `delay`
    /// of the section's decoder output.
    pub(crate) fn delay(&self) -> u64 {
        // SAFETY: reading a plain integer field of the opened, valid
        // context owned by `self.inner`.
        let d = unsafe { (*self.inner.as_ptr()).delay };
        u64::try_from(d).unwrap_or(0)
    }

    /// Feeds one packet. An error concerns this packet only; decoding can
    /// continue with the next one.
    pub(crate) fn decode(&mut self, data: &[u8]) -> Result<(), FfmpegError> {
        // An empty packet would mean end of stream to libavcodec; a
        // zero-length Ogg packet decodes to nothing, so skip it.
        if data.is_empty() {
            return Ok(());
        }
        self.inner.send_packet(&ff::Packet::copy(data))?;
        Ok(())
    }

    /// Signals the end of the stream so that buffered samples come out.
    pub(crate) fn flush(&mut self) {
        // Fails only if already flushed, which cannot happen here.
        let _ = self.inner.send_eof();
    }

    /// The next decoded frame as interleaved `f32`, or `None` when the
    /// decoder needs more input. Frames in an unsupported sample format
    /// are reported as silence of the right length rather than shifting
    /// the timeline.
    pub(crate) fn next_samples(&mut self, channels: usize) -> Option<Vec<f32>> {
        self.inner.receive_frame(&mut self.frame).ok()?;
        Some(to_interleaved_f32(&self.frame, channels))
    }
}

/// One opened libavcodec video decoder. Packets carry their decode and
/// presentation times, so each picture comes out with its presentation
/// time.
pub(crate) struct VideoDecoder {
    inner: ff::decoder::Video,
    frame: ff::frame::Video,
}

impl VideoDecoder {
    /// Opens a decoder for the stream `info` with the container's codec
    /// configuration `extradata`; times are ticks of `info.timescale`.
    pub(crate) fn new(info: &VideoStreamInfo, extradata: &[u8]) -> Result<Self, FfmpegError> {
        let (codec, timescale) = (info.codec, info.timescale);
        ff::init()?;
        // libdav1d announces its version at the "info" level each time a
        // decoder opens; only warnings and errors are worth a log line.
        use ff::util::log::{self, Level};
        let warning = i32::from(Level::Warning);
        if log::get_level().is_ok_and(|l| i32::from(l) > warning) {
            log::set_level(Level::Warning);
        }
        let found = match video_codec(codec) {
            VideoId::Id(id) => ff::decoder::find(id),
            VideoId::Name(name) => ff::decoder::find_by_name(name),
            VideoId::None => None,
        };
        let found = found.ok_or(FfmpegError::NoDecoder)?;
        let mut ctx = ff::codec::Context::new_with_codec(found);
        // SAFETY: `ptr` is the valid, exclusively owned context `ctx` just
        // allocated and not opened yet, so setting these fields is
        // allowed. The extradata buffer comes from `av_mallocz` with the
        // padding libavcodec requires and is freed by the context.
        unsafe {
            let ptr = ctx.as_mut_ptr();
            (*ptr).pkt_timebase = ff::ffi::AVRational {
                num: 1,
                den: i32::try_from(timescale).unwrap_or(i32::MAX).max(1),
            };
            // The sample entry's size, as FFmpeg's demuxer passes it (the
            // ProRes decoder warns about a "resolution change" without).
            (*ptr).width = i32::try_from(info.width).unwrap_or(0);
            (*ptr).height = i32::try_from(info.height).unwrap_or(0);
            // The sample entry's fourcc, as FFmpeg's demuxer passes it:
            // the ProRes decoder takes its profile (Proxy, LT, ...) from it.
            if let VideoCodec::Other(fourcc) = codec {
                (*ptr).codec_tag = u32::from_le_bytes(fourcc);
            }
            if !extradata.is_empty() {
                let len = extradata.len();
                let pad = ff::ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize;
                let buf = ff::ffi::av_mallocz(len + pad) as *mut u8;
                if buf.is_null() {
                    return Err(ff::Error::Other {
                        errno: ff::error::ENOMEM,
                    }
                    .into());
                }
                std::ptr::copy_nonoverlapping(extradata.as_ptr(), buf, len);
                (*ptr).extradata = buf;
                (*ptr).extradata_size = len as i32;
            }
        }
        let inner = ctx.decoder().video()?;
        Ok(VideoDecoder {
            inner,
            frame: ff::frame::Video::empty(),
        })
    }

    /// Feeds one frame's data with its decode and presentation time. An
    /// error concerns this frame only.
    pub(crate) fn decode(
        &mut self,
        data: &[u8],
        dts: MediaTime,
        pts: MediaTime,
    ) -> Result<(), FfmpegError> {
        if data.is_empty() {
            return Ok(());
        }
        let mut packet = ff::Packet::copy(data);
        packet.set_dts(Some(dts));
        packet.set_pts(Some(pts));
        self.inner.send_packet(&packet)?;
        Ok(())
    }

    /// Signals the end of the stream so that the pictures the decoder
    /// still holds (reordering delay) come out.
    pub(crate) fn flush(&mut self) {
        // Fails only if already flushed, which callers avoid.
        let _ = self.inner.send_eof();
    }

    /// The next decoded picture, or `None` when the decoder needs more
    /// input. A picture in a format that cannot be read is an error.
    pub(crate) fn next_frame(&mut self) -> Option<Result<Frame, FfmpegError>> {
        self.inner.receive_frame(&mut self.frame).ok()?;
        Some(copy_frame(&self.frame))
    }
}

/// How to find the decoder of a video codec.
enum VideoId {
    /// By codec ID.
    Id(Id),
    /// By decoder name, for codecs whose ID is newer than the oldest
    /// supported FFmpeg (the binding then lacks it).
    Name(&'static str),
    /// No FFmpeg decoder.
    None,
}

/// The FFmpeg decoder of a video codec.
fn video_codec(codec: VideoCodec) -> VideoId {
    match codec {
        VideoCodec::H264 => VideoId::Id(Id::H264),
        VideoCodec::Hevc => VideoId::Id(Id::HEVC),
        VideoCodec::Av1 => VideoId::Id(Id::AV1),
        VideoCodec::Vp9 => VideoId::Id(Id::VP9),
        VideoCodec::Mpeg4Visual => VideoId::Id(Id::MPEG4),
        VideoCodec::Other(fourcc) => match &fourcc {
            b"apco" | b"apcs" | b"apcn" | b"apch" | b"ap4h" | b"ap4x" => VideoId::Id(Id::PRORES),
            // FFmpeg 8.0 and newer.
            b"apv1" => VideoId::Name("apv"),
            b"jpeg" => VideoId::Id(Id::MJPEG),
            b"mp2v" => VideoId::Id(Id::MPEG2VIDEO),
            b"mp1v" => VideoId::Id(Id::MPEG1VIDEO),
            b"s263" | b"h263" => VideoId::Id(Id::H263),
            _ => VideoId::None,
        },
    }
}

/// Copies a decoded picture out of FFmpeg's frame.
fn copy_frame(f: &ff::frame::Video) -> Result<Frame, FfmpegError> {
    let format = f.format();
    let layout = pixel_layout(format)?;
    let mut planes = Vec::new();
    for i in 0..f.planes() {
        // SAFETY: `av_image_get_linesize` only reads the static format
        // descriptor; it returns a negative error for an invalid plane.
        let row_bytes =
            unsafe { ff::ffi::av_image_get_linesize(format.into(), f.width() as i32, i as i32) };
        let row_bytes = usize::try_from(row_bytes)
            .map_err(|_| FfmpegError::PixelFormat(layout.name.clone()))?;
        // Rows without the decoder's padding, which holds whatever the
        // buffer held before (equal pictures then compare equal).
        let (stride, rows) = (f.stride(i), f.plane_height(i) as usize);
        if row_bytes > stride {
            return Err(FfmpegError::PixelFormat(layout.name));
        }
        let data = f.data(i);
        let mut packed = Vec::with_capacity(row_bytes * rows);
        for row in 0..rows {
            packed.extend_from_slice(&data[row * stride..row * stride + row_bytes]);
        }
        planes.push(Plane {
            data: packed,
            stride: row_bytes,
            row_bytes,
            rows,
        });
    }
    if layout.components.iter().any(|c| c.plane >= planes.len()) {
        return Err(FfmpegError::PixelFormat(layout.name));
    }
    use ff::color::Space;
    let matrix = match f.color_space() {
        Space::BT709 => Matrix::Bt709,
        Space::BT2020NCL | Space::BT2020CL => Matrix::Bt2020,
        Space::BT470BG | Space::SMPTE170M | Space::FCC => Matrix::Bt601,
        // Unspecified: what players assume, BT.709 for HD pictures.
        _ if f.height() > 576 => Matrix::Bt709,
        _ => Matrix::Bt601,
    };
    Ok(Frame {
        time: f.timestamp().unwrap_or(0),
        width: f.width(),
        height: f.height(),
        layout,
        planes,
        matrix,
        full_range: f.color_range() == ff::color::Range::JPEG,
    })
}

/// The layout of a pixel format, from FFmpeg's `AVPixFmtDescriptor`.
fn pixel_layout(format: ff::format::Pixel) -> Result<PixelLayout, FfmpegError> {
    let desc = format
        .descriptor()
        .ok_or_else(|| FfmpegError::PixelFormat(format!("{format:?}")))?;
    let name = desc.name().to_owned();
    // SAFETY: `as_ptr` points to FFmpeg's static, immutable descriptor
    // table entry for `format`, valid for the whole program.
    let d = unsafe { &*desc.as_ptr() };
    // The flags' integer type depends on the bindgen target (u64, or a
    // 32-bit c_ulong on Windows).
    #[allow(clippy::unnecessary_cast)]
    let flags = d.flags as u64;
    let unsupported = (ff::ffi::AV_PIX_FMT_FLAG_PAL
        | ff::ffi::AV_PIX_FMT_FLAG_BITSTREAM
        | ff::ffi::AV_PIX_FMT_FLAG_HWACCEL
        | ff::ffi::AV_PIX_FMT_FLAG_BAYER
        | ff::ffi::AV_PIX_FMT_FLAG_FLOAT) as u64;
    let n = usize::from(d.nb_components);
    if flags & unsupported != 0 || n == 0 {
        return Err(FfmpegError::PixelFormat(name));
    }
    let rgb = flags & ff::ffi::AV_PIX_FMT_FLAG_RGB as u64 != 0;
    if rgb && n < 3 {
        return Err(FfmpegError::PixelFormat(name));
    }
    let components: Vec<Component> = d.comp[..n]
        .iter()
        .map(|c| Component {
            plane: c.plane as usize,
            step: c.step as usize,
            offset: c.offset as usize,
            shift: c.shift as u32,
            depth: c.depth as u32,
        })
        .collect();
    if components
        .iter()
        .any(|c| c.depth == 0 || c.depth > 16 || c.shift + c.depth > 16)
    {
        return Err(FfmpegError::PixelFormat(name));
    }
    Ok(PixelLayout {
        // yuvj420p and friends: the deprecated full-range formats of
        // MJPEG decoders.
        full_range: name.starts_with("yuvj"),
        name,
        // A two-component gray format (ya8) has no chroma; [`Frame`]
        // treats fewer than three components as gray.
        components,
        log2_chroma_w: u32::from(d.log2_chroma_w),
        log2_chroma_h: u32::from(d.log2_chroma_h),
        rgb,
        big_endian: flags & ff::ffi::AV_PIX_FMT_FLAG_BE as u64 != 0,
    })
}

/// The FFmpeg codec of a section, plus the coded bits per sample for PCM.
fn codec_id(section: &DecodeSection) -> Result<(Id, i32), FfmpegError> {
    Ok(match section.codec {
        Codec::Flac => (Id::FLAC, 0),
        Codec::Vorbis => (Id::VORBIS, 0),
        Codec::Opus => (Id::OPUS, 0),
        Codec::Speex => (Id::SPEEX, 0),
        Codec::Mp3 => (Id::MP3, 0),
        Codec::Aac => (Id::AAC, 0),
        Codec::Alac => (Id::ALAC, 0),
        Codec::Ac3 => (Id::AC3, 0),
        Codec::AmrNb => (Id::AMR_NB, 0),
        Codec::AmrWb => (Id::AMR_WB, 0),
        Codec::Pcm {
            encoding,
            block_align,
            big_endian,
            signed_8bit,
            ..
        } => {
            // The container size of one sample decides the decoder; WAV's
            // "valid bits" (AIFF's sample size) may be fewer, the extra low
            // bits are then zero.
            let bytes = usize::from(block_align) / usize::from(section.channels.max(1));
            let id = match (encoding, bytes, big_endian) {
                (SampleEncoding::Pcm, 1, _) if signed_8bit => Id::PCM_S8,
                (SampleEncoding::Pcm, 1, _) => Id::PCM_U8,
                (SampleEncoding::Pcm, 2, false) => Id::PCM_S16LE,
                (SampleEncoding::Pcm, 2, true) => Id::PCM_S16BE,
                (SampleEncoding::Pcm, 3, false) => Id::PCM_S24LE,
                (SampleEncoding::Pcm, 3, true) => Id::PCM_S24BE,
                (SampleEncoding::Pcm, 4, false) => Id::PCM_S32LE,
                (SampleEncoding::Pcm, 4, true) => Id::PCM_S32BE,
                (SampleEncoding::Float, 4, false) => Id::PCM_F32LE,
                (SampleEncoding::Float, 4, true) => Id::PCM_F32BE,
                (SampleEncoding::Float, 8, false) => Id::PCM_F64LE,
                (SampleEncoding::Float, 8, true) => Id::PCM_F64BE,
                (SampleEncoding::ALaw, 1, _) => Id::PCM_ALAW,
                (SampleEncoding::MuLaw, 1, _) => Id::PCM_MULAW,
                (e, b, _) => return Err(FfmpegError::PcmLayout(e, b)),
            };
            (id, (bytes * 8) as i32)
        }
    })
}

/// Converts a decoded frame to interleaved `f32`. Integer samples are
/// scaled by `2^-(bits-1)` (unsigned 8 bit offset by 128 first), which is
/// exact and matches FFmpeg's own conversion.
fn to_interleaved_f32(frame: &ff::frame::Audio, channels: usize) -> Vec<f32> {
    let n = frame.samples();
    let mut out = vec![0f32; n * channels];
    let fmt = frame.format();
    let (planar, size) = match fmt {
        Sample::U8(t) => (t == Type::Planar, 1),
        Sample::I16(t) => (t == Type::Planar, 2),
        Sample::I32(t) | Sample::F32(t) => (t == Type::Planar, 4),
        Sample::I64(t) | Sample::F64(t) => (t == Type::Planar, 8),
        Sample::None => return out,
    };
    let convert = |b: &[u8]| -> f32 {
        match fmt {
            Sample::U8(_) => (f32::from(b[0]) - 128.0) / 128.0,
            Sample::I16(_) => f32::from(i16::from_le_bytes([b[0], b[1]])) / 32_768.0,
            Sample::I32(_) => i32::from_ne_bytes([b[0], b[1], b[2], b[3]]) as f32 / 2_147_483_648.0,
            Sample::F32(_) => f32::from_ne_bytes([b[0], b[1], b[2], b[3]]),
            Sample::F64(_) => {
                f64::from_ne_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32
            }
            Sample::I64(_) => {
                (i64::from_ne_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f64
                    / 9_223_372_036_854_775_808.0) as f32
            }
            Sample::None => 0.0,
        }
    };
    if planar {
        for c in 0..channels.min(frame.planes()) {
            let plane = frame.data(c);
            for i in 0..n {
                out[i * channels + c] = convert(&plane[i * size..(i + 1) * size]);
            }
        }
    } else {
        let data = frame.data(0);
        for (i, o) in out.iter_mut().enumerate() {
            *o = convert(&data[i * size..(i + 1) * size]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A section with no packets, just enough to open a decoder.
    fn section(codec: Codec, sample_rate: u32) -> DecodeSection {
        DecodeSection {
            codec,
            sample_rate,
            channels: 2,
            extradata: Vec::new(),
            packet_ends: Vec::new(),
            keep: 0..0,
        }
    }

    /// Without a packet time base libavcodec warns "Could not update
    /// timestamps for skipped samples" for every Opus frame that loses its
    /// pre-skip, flooding test and application logs.
    #[test]
    fn decoder_has_a_per_sample_packet_time_base() {
        let decoder = Decoder::new(&section(Codec::Opus, 48_000)).expect("opus decoder");
        // SAFETY: reading a plain struct field of the opened, valid
        // context owned by `decoder.inner`.
        let tb = unsafe { (*decoder.inner.as_ptr()).pkt_timebase };
        assert_eq!((tb.num, tb.den), (1, 48_000));
    }
}
