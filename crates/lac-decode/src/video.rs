//! Decoding the pictures of a video track, one at a time, for the preview.
//!
//! Like audio, the frames come from our own demuxer (the container's
//! [`VideoIndex`]) and only libavcodec decodes them. A picture is found by
//! its presentation time: decoding starts at the clean keyframe before it
//! (in decode order; see [`VideoIndex::is_clean_start`]) and runs until the
//! decoder puts out the picture with that time. Asking for a later picture
//! of the same group of pictures continues where the last request
//! stopped, so stepping forward frame by frame decodes every frame once;
//! stepping back starts again at a keyframe.

use std::io::SeekFrom;

use lac_container::{Container, ReadSeek};
use lac_core::{MediaTime, VideoIndex};

use crate::DecodeError;
use crate::ffi;
use crate::picture::{Frame, Picture};

/// Decodes single pictures of a source's video track.
pub struct VideoDecoder {
    index: VideoIndex,
    codec_config: Vec<u8>,
    /// The decoder of the last request and how far it got.
    run: Option<Run>,
}

/// A decoder that has been fed frames from a clean start on.
struct Run {
    decoder: ffi::VideoDecoder,
    /// Decode index of the clean start it began at.
    start: usize,
    /// Decode index of the next frame to feed.
    next: usize,
    /// The end of the stream has been signalled.
    flushed: bool,
    /// The last picture it put out (pictures come in display order).
    last: Option<Frame>,
}

impl VideoDecoder {
    /// A decoder for the video track of `source`, or `None` if it has
    /// none. The decoder itself is opened on the first request.
    pub fn new(source: &dyn Container) -> Option<Self> {
        Some(Self {
            index: source.video()?.clone(),
            codec_config: source.video_codec_config().to_vec(),
            run: None,
        })
    }

    /// The frames of the track.
    pub fn index(&self) -> &VideoIndex {
        &self.index
    }

    /// The picture shown at `time` (ticks of the video timescale), in the
    /// decoder's pixel format: the first picture before the track's
    /// display range, the last one after it. `r` reads the source file.
    pub fn frame_at(
        &mut self,
        r: &mut dyn ReadSeek,
        time: MediaTime,
    ) -> Result<Frame, DecodeError> {
        let shown = self.index.display_order();
        let (Some(&first), Some(&last)) = (shown.first(), shown.last()) else {
            return Err(DecodeError::NoPicture);
        };
        let range = self.index.display_range();
        let k = if time < range.start {
            first
        } else {
            self.index.frame_at(time).unwrap_or(last)
        };
        let target = self.index.presentation_time(k);
        // The last clean start in decode order up to k: everything decoded
        // from there on is shown after it, so k decodes correctly.
        let clean = (0..=k)
            .rev()
            .find(|&i| self.index.is_clean_start(i))
            .unwrap_or(0);

        let reusable = self.run.as_ref().is_some_and(|run| {
            run.start <= k
                && clean <= run.next
                && run.last.as_ref().is_none_or(|f| f.time <= target)
        });
        if !reusable {
            let info = self.index.info();
            let decoder = ffi::VideoDecoder::new(&info, &self.codec_config).map_err(|source| {
                DecodeError::VideoSetup {
                    codec: info.codec.name(),
                    source,
                }
            })?;
            self.run = Some(Run {
                decoder,
                start: clean,
                next: clean,
                flushed: false,
                last: None,
            });
        }
        let Some(run) = self.run.as_mut() else {
            return Err(DecodeError::NoPicture);
        };
        if let Some(f) = run.last.as_ref().filter(|f| f.time == target) {
            return Ok(f.clone());
        }
        loop {
            // Pictures the decoder has ready, in display order.
            while let Some(got) = run.decoder.next_frame() {
                let frame = got.map_err(DecodeError::Video)?;
                if frame.time > target {
                    // The wanted picture never came (a damaged frame):
                    // show the one before, as a player would.
                    let shown = run.last.clone().unwrap_or_else(|| frame.clone());
                    run.last = Some(frame);
                    return Ok(shown);
                }
                let hit = frame.time == target;
                run.last = Some(frame);
                if hit {
                    return run.last.clone().ok_or(DecodeError::NoPicture);
                }
            }
            if run.next < self.index.frames().len() {
                let i = run.next;
                run.next += 1;
                let data = read_frame(&self.index, r, i)?;
                // A frame that does not decode only loses its picture.
                let _ = run.decoder.decode(
                    &data,
                    self.index.decode_time(i),
                    self.index.presentation_time(i),
                );
            } else if !run.flushed {
                run.flushed = true;
                run.decoder.flush();
            } else {
                return run.last.clone().ok_or(DecodeError::NoPicture);
            }
        }
    }

    /// The picture shown at `time` as 8-bit RGB, see
    /// [`VideoDecoder::frame_at`].
    pub fn picture_at(
        &mut self,
        r: &mut dyn ReadSeek,
        time: MediaTime,
    ) -> Result<Picture, DecodeError> {
        Ok(self.frame_at(r, time)?.to_rgb())
    }
}

/// The bytes of frame `i`.
fn read_frame(index: &VideoIndex, r: &mut dyn ReadSeek, i: usize) -> Result<Vec<u8>, DecodeError> {
    let f = &index.frames()[i];
    let mut data = vec![0; f.byte_len as usize];
    r.seek(SeekFrom::Start(f.byte_offset))
        .and_then(|_| r.read_exact(&mut data))
        .map_err(|e| DecodeError::Container(e.into()))?;
    Ok(data)
}
