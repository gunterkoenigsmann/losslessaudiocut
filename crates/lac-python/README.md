# losslessaudiocut

Cut audio recordings **without re-encoding them**: what you keep decodes to
exactly the same samples as the original. No quality loss, no generation
loss, and it is fast because nothing is decoded and encoded again.

```sh
pip install losslessaudiocut
```

The wheels contain everything, including the FFmpeg decoders the analysis
uses; nothing else needs to be installed.

Supported: WAV, AIFF, FLAC, Ogg Vorbis/Opus/Speex, MP3, MP4/M4A/3GP/MOV
(AAC, ALAC, Opus, AMR, PCM), AC-3 and Matroska/WebM (Opus). The format is
detected from the content, and the output has the format of the input.

## Command line

The package installs the `lac` command (also `python -m losslessaudiocut`):

```sh
lac info talk.mp3
lac cut talk.mp3 -o short.mp3 --delete 0..12.5 --delete 29:00..
lac cut talk.mp3 -o part2.mp3 --keep 10:00..20:00 --tag title="Part 2" --tag track=2
lac detect talk.mp3          # suggests pauses to delete and track starts
lac --help
```

## Python

```python
import losslessaudiocut as lac

info = lac.info("talk.mp3")
print(info.format, info.duration, info.tags)

# Delete ranges and join the rest ...
lac.cut("talk.mp3", "short.mp3", delete=["0..12.5", "29:00.."])
# ... or keep ranges, joined in the given order, and set tags.
result = lac.cut("talk.mp3", "part2.mp3", keep=[(600, 1200)],
                 tags={"title": "Part 2", "track": "2"})
print(result.exact, result.kept)

# Remove every suggested pause.
pauses = lac.detect("talk.mp3").pauses
lac.cut("talk.mp3", "tight.mp3", delete=[p.range for p in pauses])
```

Times are seconds (`90`, `90.5`) or strings (`"1:30"`, `"1:02:03.5"`,
`"@44100"` for sample frame 44100). A range is `(start, end)` or
`"START..END"`; leave a side out (`None`, `"5:00.."`) for the start or end
of the file. Results give positions in sample frames of the source, plus
seconds.

Lossy formats can only be cut at frame boundaries (MP3: 1152 samples).
Where a requested range does not fall on one, it is widened, never
shortened; `result.kept` tells exactly what the file contains and
`result.exact` whether that is what you asked for.

`lac.decode(path, range)` returns the decoded samples (32-bit float), with
the same decoders and timeline as cutting, e.g. to check a cut:
`lac.decode("part2.mp3")` against `lac.decode("talk.mp3", (600, 1200))`.

Errors: `ValueError` for malformed or out-of-range arguments,
`losslessaudiocut.Error` when a file cannot be read or written.

## FFmpeg and security updates

The wheels link a minimal FFmpeg statically (Windows: next to the module):
only the audio decoders, no demuxers, network or other parts. When FFmpeg
publishes a new point release, usually with security fixes, a new package
version with it follows within a week. `pip install -U losslessaudiocut`
picks it up; `lac.ffmpeg_version()` shows the bundled version.

## More

The program, its desktop GUI and the design: see the
[repository](https://github.com/gunterkoenigsmann/losslessaudiocut).
License: GPL-3.0-or-later; the bundled FFmpeg is LGPL-2.1-or-later (its
license and the source link are inside the wheel, in
`losslessaudiocut/ffmpeg/`).
