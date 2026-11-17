"""Tests of the Python package, run against the installed wheel:

    pip install losslessaudiocut-*.whl pytest && pytest crates/lac-python/tests

They check the Python layer (argument conversion, result classes, errors,
the ``lac`` command) and, with an independent reader (the standard
library's ``wave``) where one is at hand, that cuts are lossless. The
cutting itself is tested in depth by the Rust crates.
"""

from __future__ import annotations

import array
import math
import os
import shutil
import struct
import subprocess
import sys
import wave
from pathlib import Path

import pytest

import losslessaudiocut as lac

FIXTURES = Path(__file__).resolve().parents[2] / "lac-container" / "tests" / "fixtures"
MP3 = FIXTURES / "mp3_cbr128_44k.mp3"
FLAC = FIXTURES / "flac_stereo16_44k.flac"


def ramp_wav(path: Path, frames: int, rate: int = 1000) -> None:
    """16-bit mono WAV whose sample n has the value n, so a cut file shows
    exactly which samples it kept."""
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(struct.pack(f"<{frames}h", *range(frames)))


def wav_samples(path: Path) -> list[int]:
    """The samples of a 16-bit mono WAV, read by the standard library."""
    with wave.open(str(path), "rb") as w:
        assert (w.getnchannels(), w.getsampwidth()) == (1, 2)
        data = w.readframes(w.getnframes())
    return list(struct.unpack(f"<{len(data) // 2}h", data))


def test_version_and_ffmpeg() -> None:
    assert lac.__version__[0].isdigit()
    # The bundled FFmpeg (or the system's in a source build) reports itself.
    assert any(c.isdigit() for c in lac.ffmpeg_version())


def test_info_reads_format_length_and_stream() -> None:
    i = lac.info(MP3)
    assert i.format == "MP3"
    assert (i.sample_rate, i.channels) == (44100, 2)
    assert i.samples > 0
    assert i.duration == pytest.approx(i.samples / 44100)
    assert i.video is None
    assert isinstance(i.tags, dict)


def test_info_accepts_path_objects_and_strings() -> None:
    assert lac.info(str(MP3)) == lac.info(MP3)


def test_keep_ranges_in_every_time_syntax(tmp_path: Path) -> None:
    src, out = tmp_path / "ramp.wav", tmp_path / "out.wav"
    ramp_wav(src, 5000)
    # Seconds as int and float, m:ss strings, sample frames and open ends.
    r = lac.cut(src, out, keep=[(1, 1.5), "@200..@300", ("0:04", None)])
    assert wav_samples(out) == list(range(1000, 1500)) + list(range(200, 300)) + list(range(4000, 5000))
    assert r.exact
    assert r.samples == 1600
    assert r.duration == pytest.approx(1.6)
    assert [(k.start, k.end) for k in r.kept] == [(1000, 1500), (200, 300), (4000, 5000)]
    assert r.kept[0].start_seconds == 1.0


def test_delete_ranges_join_the_rest(tmp_path: Path) -> None:
    src, out = tmp_path / "ramp.wav", tmp_path / "out.wav"
    ramp_wav(src, 3000)
    lac.cut(src, out, delete=["..0.5", (2, None)])
    assert wav_samples(out) == list(range(500, 2000))


def test_a_single_range_needs_no_list(tmp_path: Path) -> None:
    src, out = tmp_path / "ramp.wav", tmp_path / "out.wav"
    ramp_wav(src, 3000)
    lac.cut(src, out, keep=(1, 2))
    assert wav_samples(out) == list(range(1000, 2000))
    lac.cut(src, out, keep="@10..@20")
    assert wav_samples(out) == list(range(10, 20))
    # Two string ranges in a tuple are two ranges, not one (start, end).
    lac.cut(src, out, keep=("@10..@20", "@30..@40"))
    assert wav_samples(out) == list(range(10, 20)) + list(range(30, 40))


def test_flac_cut_decodes_bit_for_bit(tmp_path: Path) -> None:
    out = tmp_path / "out.flac"
    r = lac.cut(FLAC, out, keep="@10000..@30000")
    assert r.exact
    # The golden rule: the kept audio decodes to exactly the source's
    # samples in that range.
    assert lac.decode(out).data == lac.decode(FLAC, "@10000..@30000").data


def test_lossy_cut_is_widened_never_shortened(tmp_path: Path) -> None:
    out = tmp_path / "out.mp3"
    r = lac.cut(MP3, out, keep=(0, 0.5))
    (k,) = r.kept
    assert (k.requested_start, k.requested_end) == (0, 22050)
    assert k.start <= k.requested_start and k.end >= k.requested_end
    assert r.exact == (k.end == 22050 and k.padding == 0)
    # Cut at a frame boundary from the start: no join, so the file decodes
    # to the source's samples of the kept range.
    got = lac.decode(out)
    assert got.samples == k.end - k.start
    assert got.data == lac.decode(MP3, f"@{k.start}..@{k.end}").data


def test_tags_are_set_and_removed(tmp_path: Path) -> None:
    src, a, b = tmp_path / "in.flac", tmp_path / "a.flac", tmp_path / "b.flac"
    shutil.copy(FLAC, src)
    lac.cut(src, a, tags={"title": "Part 2", "track": "2"})
    assert lac.info(a).tags["title"] == "Part 2"
    assert lac.info(a).tags["track"] == "2"
    lac.cut(a, b, tags={"title": ""})
    assert "title" not in lac.info(b).tags


def test_decode_returns_interleaved_floats() -> None:
    a = lac.decode(FLAC, (0, 0.1))
    assert (a.sample_rate, a.channels) == (44100, 2)
    assert a.samples == 4410
    samples = array.array("f", a.data)
    assert len(samples) == 4410 * 2
    assert all(-1.0 <= s <= 1.0 for s in samples)


def test_detect_finds_a_pause_and_a_track_start(tmp_path: Path) -> None:
    rate = 8000
    path = tmp_path / "two_tracks.wav"
    # 4 s of a 440 Hz tone, 4 s of silence, 4 s of the tone again.
    tone = [int(8000 * math.sin(2 * math.pi * 440 * n / rate)) for n in range(4 * rate)]
    samples = tone + [0] * (4 * rate) + tone
    with wave.open(str(path), "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes(struct.pack(f"<{len(samples)}h", *samples))

    d = lac.detect(path, min_gap=2)
    assert d.sample_rate == rate
    (p,) = [p for p in d.pauses if p.kind == "silence"]
    assert p.start_seconds == pytest.approx(4, abs=0.2)
    assert p.end_seconds == pytest.approx(8, abs=0.2)
    assert any(t.reason == "gap" and 4 <= t.seconds <= 8.2 for t in d.tracks)

    # A pause's range can be handed straight to cut().
    out = tmp_path / "tight.wav"
    r = lac.cut(path, out, delete=[p.range])
    assert r.samples == len(samples) - (p.end - p.start)


def test_bad_arguments_raise_value_or_type_errors(tmp_path: Path) -> None:
    out = tmp_path / "out.mp3"
    with pytest.raises(ValueError, match="either delete or keep"):
        lac.cut(MP3, out, delete=["0..1"], keep=["1..2"])
    with pytest.raises(ValueError, match="unknown tag field"):
        lac.cut(MP3, out, tags={"composer": "x"})
    with pytest.raises(ValueError):
        lac.cut(MP3, out, keep="0..1:00:00")  # past the end
    with pytest.raises(ValueError):
        lac.cut(MP3, out, keep="1:2:3:4..")  # no such time syntax
    with pytest.raises(ValueError):
        lac.cut(MP3, out, keep=(-1, 1))
    with pytest.raises(TypeError):
        lac.cut(MP3, out, keep=(True, 1))
    with pytest.raises(ValueError, match="input"):
        lac.cut(MP3, MP3, keep=(0, 1))


def test_unreadable_files_raise_lac_error(tmp_path: Path) -> None:
    with pytest.raises(lac.Error, match="cannot open"):
        lac.info(tmp_path / "missing.mp3")
    junk = tmp_path / "junk.mp3"
    junk.write_bytes(b"this is no audio file" * 10)
    with pytest.raises(lac.Error):
        lac.info(junk)
    assert issubclass(lac.Error, Exception)


def run_lac(*args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [sys.executable, "-m", "losslessaudiocut", *args],
        capture_output=True,
        text=True,
        check=False,
    )


def test_cli_runs_as_python_module(tmp_path: Path) -> None:
    p = run_lac("info", str(MP3))
    assert p.returncode == 0, p.stderr
    assert "format:      MP3" in p.stdout

    out = tmp_path / "cut.mp3"
    p = run_lac("cut", str(MP3), "-o", str(out), "--delete", "0.5..1")
    assert p.returncode == 0, p.stderr
    assert out.stat().st_size > 0


def test_cli_reports_errors_with_exit_status() -> None:
    p = run_lac("no-such-command")
    assert p.returncode == 2
    assert "Usage: lac" in p.stderr
    p = run_lac("info", "/no/such/file.mp3")
    assert p.returncode == 1
    assert p.stderr.startswith("Error: cannot open")


def test_cli_entry_point_is_installed() -> None:
    # The `lac` script that pip puts next to the interpreter.
    scripts = Path(sys.executable).parent
    lac_cmd = shutil.which("lac", path=str(scripts)) or shutil.which("lac")
    assert lac_cmd, "the lac command is not installed"
    p = subprocess.run([lac_cmd, "--version"], capture_output=True, text=True, check=False)
    assert p.returncode == 0
    assert p.stdout.startswith("lac ")


def test_files_with_non_ascii_names(tmp_path: Path) -> None:
    src = tmp_path / "Aufnahme ä ö ü.wav"
    ramp_wav(src, 100)
    out = tmp_path / "Schnitt ß.wav"
    lac.cut(os.fspath(src), out, keep="@10..@20")
    assert wav_samples(out) == list(range(10, 20))
