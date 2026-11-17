"""Cut audio recordings without re-encoding them.

What is kept decodes to exactly the same samples as the original: lossy
formats (MP3, AAC, Opus, Vorbis, Speex, AMR, AC-3) are cut at their packet
structure and never decoded and encoded again; FLAC and PCM may re-encode
a boundary frame, which is lossless itself. The format is detected from the
content: WAV, AIFF, FLAC, Ogg Vorbis/Opus/Speex, MP3, MP4/M4A/3GP/MOV (AAC,
ALAC, Opus, AMR, PCM), AC-3 and Matroska/WebM (Opus).

    import losslessaudiocut as lac

    lac.info("talk.mp3").duration              # 1834.2 (seconds)
    lac.cut("talk.mp3", "short.mp3", delete=["0..12.5", "29:00.."])
    lac.cut("talk.mp3", "part2.mp3", keep=[(600, 1200)],
            tags={"title": "Part 2", "track": "2"})
    for p in lac.detect("talk.mp3").pauses:     # suggested pauses
        print(p.start_seconds, p.end_seconds, p.kind)

Times are seconds (``90``, ``90.5``) or strings: ``"1:30"``,
``"1:02:03.5"`` or ``"@44100"`` for an exact sample frame. A range is a
``(start, end)`` tuple or a string ``"START..END"``; ``None`` or an empty
side means the start or end of the file. Results report positions as
sample frames (``int``) of the source, plus seconds for convenience.

The same functionality is available as the command line tool ``lac``
(``lac --help``), which this package installs.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Mapping, Sequence, Union

from . import _native
from ._native import Error, ffmpeg_version

__all__ = [
    "Error",
    "Info",
    "KeptRange",
    "CutResult",
    "Pause",
    "TrackStart",
    "Detection",
    "Audio",
    "info",
    "cut",
    "detect",
    "decode",
    "ffmpeg_version",
    "__version__",
]

__version__: str = _native.__version__

#: A point in time: seconds (``int`` or ``float``) or a string in the
#: CLI's syntax (``"90.5"``, ``"1:30"``, ``"1:02:03.5"``, ``"@44100"``).
Time = Union[int, float, str]
#: A range: ``"START..END"`` or ``(start, end)``; ``None`` (or an empty side
#: of the string) means the start or the end of the file.
TimeRange = Union[str, "tuple[Time | None, Time | None]"]
#: A file name.
PathLike = Union[str, "os.PathLike[str]"]


@dataclass(frozen=True)
class Info:
    """What :func:`info` found out about a file."""

    #: Container and codec, e.g. ``"MP3"``.
    format: str
    #: Codec details, e.g. bit rate and encoder delay.
    details: str
    #: Sample frames per second.
    sample_rate: int
    #: Number of audio channels.
    channels: int
    #: Length in sample frames.
    samples: int
    #: A description of the video track, if there is one.
    video: str | None
    #: The common tags (``title``, ``artist``, ``album``, ``album_artist``,
    #: ``track``, ``tracktotal``, ``date``, ``genre``, ``comment``) that are set.
    tags: dict[str, str]
    #: All other tags as ``(key, value)`` pairs, as stored in the file.
    extra_tags: list[tuple[str, str]]
    #: Problems found while reading (damaged frames and the like).
    warnings: list[str]
    #: How exactly this format can be cut, e.g. where cuts snap to frames.
    notes: list[str]

    @property
    def duration(self) -> float:
        """Length in seconds."""
        return self.samples / self.sample_rate if self.sample_rate else 0.0


@dataclass(frozen=True)
class KeptRange:
    """One stretch of the source that ended up in the cut file.

    Positions are sample frames of the *source*.
    """

    #: The range that was asked for.
    requested_start: int
    requested_end: int
    #: The range the file contains: the requested one, or widened to frame
    #: boundaries where the format cannot cut exactly.
    start: int
    end: int
    #: Extra sample frames that play around this range (MP3 encoder delay
    #: or padding that a join cannot hide).
    padding: int
    #: True if the file contains exactly the requested samples.
    exact: bool
    #: Sample rate, to convert the positions to seconds.
    sample_rate: int

    @property
    def start_seconds(self) -> float:
        """:attr:`start` in seconds."""
        return self.start / self.sample_rate

    @property
    def end_seconds(self) -> float:
        """:attr:`end` in seconds."""
        return self.end / self.sample_rate


@dataclass(frozen=True)
class CutResult:
    """What :func:`cut` wrote."""

    #: The file written.
    output: str
    #: Sample rate of source and output.
    sample_rate: int
    #: Length of the output in sample frames.
    samples: int
    #: The kept parts of the source, in output order.
    kept: list[KeptRange]
    #: Remarks about the cut, e.g. join points of lossy formats.
    notes: list[str]

    @property
    def duration(self) -> float:
        """Length of the output in seconds."""
        return self.samples / self.sample_rate

    @property
    def exact(self) -> bool:
        """True if every kept part is exactly the requested range."""
        return all(k.exact for k in self.kept)


@dataclass(frozen=True)
class Pause:
    """A quiet or noise-only stretch that :func:`detect` suggests deleting."""

    #: Start and end in sample frames.
    start: int
    end: int
    #: ``"silence"`` or ``"noise"`` (a flat spectrum: hiss, wind, murmur).
    kind: str
    #: Its level in dBFS.
    level_db: float
    sample_rate: int

    @property
    def start_seconds(self) -> float:
        """:attr:`start` in seconds."""
        return self.start / self.sample_rate

    @property
    def end_seconds(self) -> float:
        """:attr:`end` in seconds."""
        return self.end / self.sample_rate

    @property
    def range(self) -> str:
        """The pause as a range for :func:`cut`, exact to the sample."""
        return f"@{self.start}..@{self.end}"


@dataclass(frozen=True)
class TrackStart:
    """Where :func:`detect` suggests a new track begins."""

    #: The position in sample frames.
    position: int
    #: Why: ``"gap"`` (after a long pause), ``"timbre"`` or ``"key"``.
    reason: str
    #: 0 to 1; higher is more certain.
    confidence: float
    sample_rate: int
    #: For ``"gap"``: the pause before the track, in sample frames.
    gap_start: int | None = None
    gap_end: int | None = None
    #: For ``"timbre"``: how much the timbre changes, in dB.
    novelty_db: float | None = None
    #: For ``"key"``: how far key or harmony move.
    key_distance: float | None = None

    @property
    def seconds(self) -> float:
        """:attr:`position` in seconds."""
        return self.position / self.sample_rate


@dataclass(frozen=True)
class Detection:
    """Pauses and track starts that :func:`detect` suggests."""

    sample_rate: int
    #: Levels below this (dBFS) count as quiet.
    quiet_db: float
    #: The estimated noise floor and the level of loud parts (dBFS).
    floor_db: float
    loud_db: float
    pauses: list[Pause]
    tracks: list[TrackStart]
    #: Problems found while decoding.
    warnings: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class Audio:
    """Decoded samples, from :func:`decode`."""

    sample_rate: int
    channels: int
    #: Interleaved 32-bit float samples in -1.0..1.0, in machine byte
    #: order: ``array.array("f", audio.data)`` or
    #: ``numpy.frombuffer(audio.data, numpy.float32).reshape(-1, audio.channels)``.
    data: bytes

    @property
    def samples(self) -> int:
        """Number of sample frames."""
        return len(self.data) // (4 * self.channels) if self.channels else 0


def _time(t: Time | None) -> str:
    """One side of a range in the CLI's syntax."""
    if t is None:
        return ""
    if isinstance(t, bool):
        # bool is an int subclass; True as "1 second" is surely a mistake.
        raise TypeError(f"a time must be seconds or a string, not {t!r}")
    if isinstance(t, (int, float)):
        if not t >= 0 or t == float("inf"):
            raise ValueError(f"a time must be finite and not negative, got {t!r}")
        if isinstance(t, int):
            return str(t)
        # The CLI syntax takes up to nine decimals (nanoseconds), far below
        # a sample at any rate; trailing zeros are dropped.
        return f"{t:.9f}".rstrip("0").rstrip(".")
    if isinstance(t, str):
        return t
    raise TypeError(f"a time must be seconds or a string, not {type(t).__name__}")


def _range(r: TimeRange) -> str:
    """A range in the CLI's ``START..END`` syntax."""
    if isinstance(r, str):
        return r
    try:
        start, end = r
    except (TypeError, ValueError):
        raise TypeError(f"a range must be 'START..END' or (start, end), not {r!r}") from None
    return f"{_time(start)}..{_time(end)}"


def _is_time(t: object) -> bool:
    """True for something that is one side of a range, not a whole range."""
    return t is None or isinstance(t, (int, float)) or (isinstance(t, str) and ".." not in t)


def _ranges(rs: Sequence[TimeRange] | TimeRange | None) -> list[str]:
    """Ranges in the CLI's syntax; a single range may come without a list."""
    if rs is None:
        return []
    if isinstance(rs, str) or (isinstance(rs, tuple) and len(rs) == 2 and all(map(_is_time, rs))):
        return [_range(rs)]
    return [_range(r) for r in rs]


def info(path: PathLike) -> Info:
    """Reads format, length, tags and warnings of an audio file.

    Raises :class:`Error` if the file cannot be read or is no supported
    audio file.
    """
    d = _native.info(os.fspath(path))
    return Info(
        format=d["format"],
        details=d["details"],
        sample_rate=d["sample_rate"],
        channels=d["channels"],
        samples=d["samples"],
        video=d["video"],
        tags=dict(d["tags"]),
        extra_tags=[tuple(kv) for kv in d["extra_tags"]],
        warnings=list(d["warnings"]),
        notes=list(d["notes"]),
    )


def cut(
    input: PathLike,
    output: PathLike,
    *,
    delete: Sequence[TimeRange] | TimeRange | None = None,
    keep: Sequence[TimeRange] | TimeRange | None = None,
    tags: Mapping[str, str] | None = None,
) -> CutResult:
    """Cuts ``input`` into ``output`` without re-encoding.

    Give either ``delete`` (ranges to remove; the rest is joined) or
    ``keep`` (ranges to keep, joined in the given order). Ranges refer to
    the original file. With neither, the whole file is copied, which is
    useful together with ``tags``.

    ``tags`` sets tags of the output (``title``, ``artist``, ``album``,
    ``album_artist``, ``track``, ``tracktotal``, ``date``, ``genre``,
    ``comment``); an empty value removes one. Other tags are copied.

    The output has the format of the input; ``output`` must be a different
    file. Where a format cannot cut at the exact sample (some lossy formats
    only at frame boundaries), the kept range is widened, never shortened;
    :attr:`CutResult.kept` says what the file contains.

    Raises :class:`ValueError` for malformed ranges or tags and ranges
    outside the file, :class:`Error` if reading or writing fails.
    """
    delete_ranges = _ranges(delete)
    keep_ranges = _ranges(keep)
    if delete_ranges and keep_ranges:
        raise ValueError("give either delete or keep, not both")
    tag_pairs = [(str(k), str(v)) for k, v in (tags or {}).items()]
    d = _native.cut(os.fspath(input), os.fspath(output), delete_ranges, keep_ranges, tag_pairs)
    rate = d["sample_rate"]
    return CutResult(
        output=os.fspath(output),
        sample_rate=rate,
        samples=d["samples"],
        kept=[KeptRange(sample_rate=rate, **k) for k in d["kept"]],
        notes=list(d["notes"]),
    )


_DETECT_DEFAULTS = _native.detect_defaults()


def detect(
    path: PathLike,
    *,
    min_gap: float | None = None,
    min_pause: float | None = None,
    timbre: bool = True,
    key: bool = False,
) -> Detection:
    """Suggests pauses to delete and where new tracks start.

    Pauses are stretches that are quiet or only noise. A new track is
    suggested after each pause of at least ``min_gap`` seconds and, unless
    ``timbre`` is false, where the timbre changes without a pause. ``key``
    also suggests track starts at changes of key or harmony (off by
    default: on real music it found few extra track starts but many inside
    songs). Pauses shorter than ``min_pause`` seconds are not listed.

    The file is decoded once, which takes a moment for long recordings.
    """
    d = _native.detect(
        os.fspath(path),
        float(_DETECT_DEFAULTS["min_gap"] if min_gap is None else min_gap),
        float(_DETECT_DEFAULTS["min_pause"] if min_pause is None else min_pause),
        bool(timbre),
        bool(key),
    )
    rate = d["sample_rate"]
    return Detection(
        sample_rate=rate,
        quiet_db=d["quiet_db"],
        floor_db=d["floor_db"],
        loud_db=d["loud_db"],
        pauses=[Pause(sample_rate=rate, **p) for p in d["pauses"]],
        tracks=[TrackStart(sample_rate=rate, **t) for t in d["tracks"]],
        warnings=list(d["warnings"]),
    )


def decode(path: PathLike, range: TimeRange | None = None) -> Audio:
    """Decodes a file, or ``range`` of it, to 32-bit float samples.

    Uses the same decoders and timeline as cutting, so
    ``decode(cut_file)`` can be compared with ``decode(source, range)`` to
    check a cut.
    """
    rate, channels, data = _native.decode(os.fspath(path), None if range is None else _range(range))
    return Audio(sample_rate=rate, channels=channels, data=data)
