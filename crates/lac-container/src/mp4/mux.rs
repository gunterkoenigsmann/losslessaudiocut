//! Writing the `moov` box of a cut: one `trak` per output track, and the
//! layout of their samples in `mdat`.
//!
//! An audio-only cut has one sound track (track 1). A video cut adds a
//! video track (track 2) whose samples are interleaved with the audio in
//! chunks of about half a second, ordered by time, so players read both
//! tracks from nearby places in the file (ISO/IEC 14496-12 does not
//! require interleaving, but every recorder writes it).

use std::ops::Range;

use super::boxes;

/// Audio samples per chunk (ffmpeg writes about one second's worth; any
/// count is valid).
pub(super) const CHUNK_SAMPLES: usize = 64;

/// Video frames per chunk at most.
const CHUNK_FRAMES: usize = 64;

/// What kind of track a [`TrakOut`] is.
#[derive(Debug, Clone)]
pub(super) enum Handler {
    /// A sound track.
    Sound,
    /// A video track, with the source's `tkhd` display matrix (9 32-bit
    /// fields) and display size (16.16 width and height), copied.
    Video {
        /// The display matrix, as stored.
        matrix: [u8; 36],
        /// Width and height, as stored.
        size: [u8; 8],
    },
}

/// One track of an output file, ready for its `trak` box.
#[derive(Debug, Clone)]
pub(super) struct TrakOut {
    /// Track ID, from 1.
    pub id: u32,
    /// Sound or video.
    pub handler: Handler,
    /// Media timescale (ticks per second of all sample times).
    pub timescale: u32,
    /// ISO 639-2 language as packed in `mdhd`.
    pub language: u16,
    /// The `stsd` box, header included.
    pub stsd: Vec<u8>,
    /// Duration of each sample, in decode order.
    pub durations: Vec<u32>,
    /// Size of each sample in bytes.
    pub sizes: Vec<u32>,
    /// Composition offset of each sample; empty when all are 0.
    pub ctts: Vec<i32>,
    /// Sync flags; `None` when every sample is a sync sample.
    pub sync: Option<Vec<bool>>,
    /// Every sample needs its predecessor (AAC): a `roll` sample group
    /// with distance -1.
    pub roll: bool,
    /// A leading empty edit (movie timescale): how much later than the
    /// movie's start the track begins; 0 for none.
    pub empty_edit: u64,
    /// The edit list's media time (media timescale).
    pub media_time: i64,
    /// The edit list's duration (movie timescale).
    pub edit_duration: u64,
}

impl TrakOut {
    /// Sum of the sample durations.
    fn media_duration(&self) -> u64 {
        self.durations.iter().map(|&d| u64::from(d)).sum()
    }
}

/// A run of samples of one track stored together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Chunk {
    /// Index into the tracks.
    pub track: usize,
    /// Sample indexes.
    pub samples: Range<usize>,
}

/// The chunks of all tracks in file order: each track cut into chunks
/// (audio [`CHUNK_SAMPLES`] samples, video at most half a second or
/// [`CHUNK_FRAMES`] frames), merged by the decode time of their first
/// sample. For a single track that is simply its chunks in order.
pub(super) fn layout(tracks: &[TrakOut]) -> Vec<Chunk> {
    // (start time as a fraction num/den, chunk)
    let mut all: Vec<(u64, u64, Chunk)> = Vec::new();
    for (t, trak) in tracks.iter().enumerate() {
        let n = trak.sizes.len();
        let mut start = 0usize;
        let mut time = 0u64;
        while start < n {
            let mut end = start;
            let mut span = 0u64;
            match trak.handler {
                Handler::Sound => end = (start + CHUNK_SAMPLES).min(n),
                Handler::Video { .. } => {
                    while end < n
                        && end - start < CHUNK_FRAMES
                        && (end == start || span < u64::from(trak.timescale) / 2)
                    {
                        span += u64::from(trak.durations[end]);
                        end += 1;
                    }
                }
            }
            all.push((
                time,
                u64::from(trak.timescale),
                Chunk {
                    track: t,
                    samples: start..end,
                },
            ));
            time += trak.durations[start..end]
                .iter()
                .map(|&d| u64::from(d))
                .sum::<u64>();
            start = end;
        }
    }
    // Stable: chunks of one track keep their order, and at equal times the
    // track listed first comes first.
    all.sort_by(|a, b| {
        (u128::from(a.0) * u128::from(b.1)).cmp(&(u128::from(b.0) * u128::from(a.1)))
    });
    all.into_iter().map(|(_, _, c)| c).collect()
}

/// The `moov` box for `tracks`, whose chunks (in each track's order) start
/// at the file offsets `chunk_offsets[track]`. The movie timescale is
/// `movie_timescale`; `udta` is appended as it is.
pub(super) fn moov(
    movie_timescale: u32,
    tracks: &[TrakOut],
    chunks: &[Chunk],
    chunk_offsets: &[Vec<u64>],
    large: bool,
    udta: &[u8],
) -> Vec<u8> {
    let fits = |v: u64| u32::try_from(v).is_ok();
    let v1 = tracks
        .iter()
        .any(|t| !fits(t.media_duration()) || !fits(t.empty_edit + t.edit_duration));
    let time_fields = |out: &mut Vec<u8>, duration: u64| {
        if v1 {
            out.extend([0; 16]); // creation and modification time
            return duration.to_be_bytes().to_vec();
        }
        out.extend([0; 8]);
        (duration as u32).to_be_bytes().to_vec()
    };
    let version = u8::from(v1);
    let identity: [u32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];
    let movie_duration = tracks
        .iter()
        .map(|t| t.empty_edit + t.edit_duration)
        .max()
        .unwrap_or(0);

    let mut mvhd = Vec::new();
    let d = time_fields(&mut mvhd, movie_duration);
    mvhd.extend(movie_timescale.to_be_bytes());
    mvhd.extend(d);
    mvhd.extend(0x1_0000u32.to_be_bytes()); // rate 1.0
    mvhd.extend(0x0100u16.to_be_bytes()); // volume 1.0
    mvhd.extend([0; 10]);
    identity.iter().for_each(|m| mvhd.extend(m.to_be_bytes()));
    mvhd.extend([0; 24]); // pre_defined
    mvhd.extend((tracks.len() as u32 + 1).to_be_bytes()); // next_track_ID

    let mut moov = Vec::new();
    boxes::put_full(&mut moov, b"mvhd", version, 0, &mvhd);
    for (i, t) in tracks.iter().enumerate() {
        // Samples per chunk, in this track's chunk order.
        let per_chunk: Vec<u32> = chunks
            .iter()
            .filter(|c| c.track == i)
            .map(|c| (c.samples.end - c.samples.start) as u32)
            .collect();
        let trak = trak(
            t,
            version,
            &time_fields,
            &per_chunk,
            &chunk_offsets[i],
            large,
        );
        boxes::put(&mut moov, b"trak", &trak);
    }
    moov.extend_from_slice(udta);
    let mut out = Vec::new();
    boxes::put(&mut out, b"moov", &moov);
    out
}

/// The body of one `trak` box.
fn trak(
    t: &TrakOut,
    version: u8,
    time_fields: &dyn Fn(&mut Vec<u8>, u64) -> Vec<u8>,
    per_chunk: &[u32],
    offsets: &[u64],
    large: bool,
) -> Vec<u8> {
    let v1 = version == 1;
    let identity: [u32; 9] = [0x1_0000, 0, 0, 0, 0x1_0000, 0, 0, 0, 0x4000_0000];
    // tkhd (enabled, in movie).
    let mut tkhd = Vec::new();
    let d = time_fields(&mut tkhd, t.empty_edit + t.edit_duration);
    tkhd.extend(t.id.to_be_bytes());
    tkhd.extend([0; 4]);
    tkhd.extend(d);
    tkhd.extend([0; 8]);
    tkhd.extend(0u16.to_be_bytes()); // layer
    match &t.handler {
        Handler::Sound => {
            tkhd.extend(1u16.to_be_bytes()); // alternate group
            tkhd.extend(0x0100u16.to_be_bytes()); // volume
            tkhd.extend([0; 2]);
            identity.iter().for_each(|m| tkhd.extend(m.to_be_bytes()));
            tkhd.extend([0; 8]); // width, height
        }
        Handler::Video { matrix, size } => {
            tkhd.extend(0u16.to_be_bytes()); // alternate group
            tkhd.extend(0u16.to_be_bytes()); // volume
            tkhd.extend([0; 2]);
            tkhd.extend(matrix);
            tkhd.extend(size);
        }
    }

    // elst (ISO/IEC 14496-12 8.6.6): an optional empty edit (media time
    // -1) that delays the track, then one edit with the trims.
    let mut edits = Vec::new();
    if t.empty_edit > 0 {
        edits.push((t.empty_edit, -1));
    }
    edits.push((t.edit_duration, t.media_time));
    let mut elst = (edits.len() as u32).to_be_bytes().to_vec();
    for (duration, media_time) in edits {
        if v1 {
            elst.extend(duration.to_be_bytes());
            elst.extend(media_time.to_be_bytes());
        } else {
            elst.extend((duration as u32).to_be_bytes());
            elst.extend((media_time as i32).to_be_bytes());
        }
        elst.extend(0x1_0000u32.to_be_bytes()); // media rate 1.0
    }
    let mut edts = Vec::new();
    boxes::put_full(&mut edts, b"elst", version, 0, &elst);

    let mut mdhd = Vec::new();
    let d = time_fields(&mut mdhd, t.media_duration());
    mdhd.extend(t.timescale.to_be_bytes());
    mdhd.extend(d);
    mdhd.extend(t.language.to_be_bytes());
    mdhd.extend([0; 2]);
    let mut hdlr = 0u32.to_be_bytes().to_vec();
    let mut media_header = Vec::new();
    match t.handler {
        Handler::Sound => {
            hdlr.extend(b"soun");
            hdlr.extend([0; 12]);
            hdlr.extend(b"SoundHandler\0");
            boxes::put_full(&mut media_header, b"smhd", 0, 0, &[0; 4]);
        }
        Handler::Video { .. } => {
            hdlr.extend(b"vide");
            hdlr.extend([0; 12]);
            hdlr.extend(b"VideoHandler\0");
            // graphicsmode copy, opcolor 0; flags 1 as the standard wants.
            boxes::put_full(&mut media_header, b"vmhd", 0, 1, &[0; 8]);
        }
    }

    let mut url = Vec::new();
    boxes::put_full(&mut url, b"url ", 0, 1, &[]);
    let mut dref = 1u32.to_be_bytes().to_vec();
    dref.extend(url);
    let mut dinf = Vec::new();
    boxes::put_full(&mut dinf, b"dref", 0, 0, &dref);

    let mut stbl = t.stsd.clone();
    // stts: runs of equal durations.
    let mut stts = Vec::new();
    put_runs(&mut stts, t.durations.iter().map(|&d| d.to_be_bytes()));
    boxes::put_full(&mut stbl, b"stts", 0, 0, &stts);
    if t.ctts.iter().any(|&c| c != 0) {
        // ctts (8.6.1.3): version 1 when an offset is negative.
        let negative = t.ctts.iter().any(|&c| c < 0);
        let mut ctts = Vec::new();
        put_runs(&mut ctts, t.ctts.iter().map(|&c| c.to_be_bytes()));
        boxes::put_full(&mut stbl, b"ctts", u8::from(negative), 0, &ctts);
    }
    if let Some(sync) = &t.sync {
        // stss (8.6.2): numbers of the sync samples, from 1.
        let numbers: Vec<u32> = (0..sync.len())
            .filter(|&i| sync[i])
            .map(|i| i as u32 + 1)
            .collect();
        let mut stss = (numbers.len() as u32).to_be_bytes().to_vec();
        numbers.iter().for_each(|n| stss.extend(n.to_be_bytes()));
        boxes::put_full(&mut stbl, b"stss", 0, 0, &stss);
    }
    // stsc: runs of chunks with the same sample count.
    let mut stsc_entries: Vec<(u32, u32)> = Vec::new();
    for (c, &n) in per_chunk.iter().enumerate() {
        if stsc_entries.last().is_none_or(|e| e.1 != n) {
            stsc_entries.push((c as u32 + 1, n));
        }
    }
    let mut stsc = (stsc_entries.len() as u32).to_be_bytes().to_vec();
    for (c, s) in stsc_entries {
        stsc.extend(c.to_be_bytes());
        stsc.extend(s.to_be_bytes());
        stsc.extend(1u32.to_be_bytes()); // sample description 1
    }
    boxes::put_full(&mut stbl, b"stsc", 0, 0, &stsc);
    let n = t.sizes.len();
    let mut stsz = 0u32.to_be_bytes().to_vec();
    stsz.extend((n as u32).to_be_bytes());
    t.sizes.iter().for_each(|s| stsz.extend(s.to_be_bytes()));
    boxes::put_full(&mut stbl, b"stsz", 0, 0, &stsz);
    let mut co = (offsets.len() as u32).to_be_bytes().to_vec();
    for &off in offsets {
        if large {
            co.extend(off.to_be_bytes());
        } else {
            co.extend((off as u32).to_be_bytes());
        }
    }
    boxes::put_full(&mut stbl, if large { b"co64" } else { b"stco" }, 0, 0, &co);
    if t.roll && n > 0 {
        // ISO/IEC 14496-12 10.1: every AAC sample needs its predecessor
        // (roll distance -1), as ffmpeg writes it, so seeking players
        // decode one sample ahead.
        let mut sgpd = b"roll".to_vec();
        sgpd.extend(2u32.to_be_bytes()); // default_length
        sgpd.extend(1u32.to_be_bytes());
        sgpd.extend((-1i16).to_be_bytes());
        boxes::put_full(&mut stbl, b"sgpd", 1, 0, &sgpd);
        let mut sbgp = b"roll".to_vec();
        sbgp.extend(1u32.to_be_bytes());
        sbgp.extend((n as u32).to_be_bytes());
        sbgp.extend(1u32.to_be_bytes());
        boxes::put_full(&mut stbl, b"sbgp", 0, 0, &sbgp);
    }

    let mut minf = media_header;
    boxes::put(&mut minf, b"dinf", &dinf);
    boxes::put(&mut minf, b"stbl", &stbl);
    let mut mdia = Vec::new();
    boxes::put_full(&mut mdia, b"mdhd", version, 0, &mdhd);
    boxes::put_full(&mut mdia, b"hdlr", 0, 0, &hdlr);
    boxes::put(&mut mdia, b"minf", &minf);
    let mut out = Vec::new();
    boxes::put_full(&mut out, b"tkhd", version, 3, &tkhd);
    boxes::put(&mut out, b"edts", &edts);
    boxes::put(&mut out, b"mdia", &mdia);
    out
}

/// Appends an entry count and (count, value) runs of equal values, as
/// `stts` and `ctts` store them.
fn put_runs(out: &mut Vec<u8>, values: impl Iterator<Item = [u8; 4]>) {
    let mut runs: Vec<(u32, [u8; 4])> = Vec::new();
    for v in values {
        match runs.last_mut() {
            Some((n, last)) if *last == v => *n += 1,
            _ => runs.push((1, v)),
        }
    }
    out.extend((runs.len() as u32).to_be_bytes());
    for (n, v) in runs {
        out.extend(n.to_be_bytes());
        out.extend(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(handler: Handler, timescale: u32, durations: Vec<u32>) -> TrakOut {
        TrakOut {
            id: 1,
            handler,
            timescale,
            language: 0,
            stsd: Vec::new(),
            sizes: vec![1; durations.len()],
            durations,
            ctts: Vec::new(),
            sync: None,
            roll: false,
            empty_edit: 0,
            media_time: 0,
            edit_duration: 0,
        }
    }

    #[test]
    fn chunks_interleave_by_time() {
        // 200 AAC frames at 48 kHz (64 per chunk = 1.365 s) and 120 video
        // frames at 30 fps (15 per chunk = 0.5 s) in a 90 kHz timescale.
        let audio = track(Handler::Sound, 48_000, vec![1024; 200]);
        let video = track(
            Handler::Video {
                matrix: [0; 36],
                size: [0; 8],
            },
            90_000,
            vec![3000; 120],
        );
        let chunks = layout(&[audio, video]);
        let starts: Vec<(usize, usize)> =
            chunks.iter().map(|c| (c.track, c.samples.start)).collect();
        assert_eq!(
            starts,
            [
                (0, 0),
                (1, 0),
                (1, 15),
                (1, 30),
                (0, 64),
                (1, 45),
                (1, 60),
                (1, 75),
                (0, 128),
                (1, 90),
                (1, 105),
                (0, 192),
            ]
        );
        // Every sample of every track is in exactly one chunk.
        for t in 0..2 {
            let mut next = 0;
            for c in chunks.iter().filter(|c| c.track == t) {
                assert_eq!(c.samples.start, next);
                next = c.samples.end;
            }
            assert_eq!(next, if t == 0 { 200 } else { 120 });
        }
    }
}
