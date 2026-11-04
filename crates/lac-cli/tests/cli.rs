//! End-to-end tests of the `lac` binary.

use std::path::PathBuf;
use std::process::Command;

fn lac() -> Command {
    Command::new(env!("CARGO_BIN_EXE_lac"))
}

/// A fresh temporary directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lac-cli-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

/// 16-bit mono WAV at 1 kHz whose sample *n* has the value *n*, so the
/// output reveals exactly which samples were kept.
fn ramp_wav(frames: u16) -> Vec<u8> {
    let data: Vec<u8> = (0..frames).flat_map(|n| n.to_le_bytes()).collect();
    let mut f = b"RIFF".to_vec();
    f.extend((36 + data.len() as u32).to_le_bytes());
    f.extend(b"WAVEfmt ");
    f.extend(16u32.to_le_bytes());
    f.extend(1u16.to_le_bytes()); // PCM
    f.extend(1u16.to_le_bytes()); // mono
    f.extend(1000u32.to_le_bytes()); // 1 kHz
    f.extend(2000u32.to_le_bytes()); // byte rate
    f.extend(2u16.to_le_bytes()); // block align
    f.extend(16u16.to_le_bytes()); // bits
    f.extend(b"data");
    f.extend((data.len() as u32).to_le_bytes());
    f.extend(data);
    f
}

/// Reads back the sample values of a file written by `ramp_wav` + cutting.
fn samples(wav: &[u8]) -> Vec<u16> {
    wav[44..]
        .chunks(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect()
}

#[test]
fn delete_ranges_are_removed_and_rest_joined() {
    let d = temp_dir("delete");
    let input = d.join("in.wav");
    let output = d.join("out.wav");
    std::fs::write(&input, ramp_wav(5000)).expect("write input");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--delete", "1..2", "--delete", "@4500.."])
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = samples(&std::fs::read(&output).expect("output"));
    let want: Vec<u16> = (0..1000).chain(2000..4500).collect();
    assert_eq!(got, want);
}

#[test]
fn keep_ranges_are_joined_in_order() {
    let d = temp_dir("keep");
    let input = d.join("in.wav");
    let output = d.join("out.wav");
    std::fs::write(&input, ramp_wav(3000)).expect("write input");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--keep", "2..2.5", "--keep", "0.1..0.2"])
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let got = samples(&std::fs::read(&output).expect("output"));
    let want: Vec<u16> = (2000..2500).chain(100..200).collect();
    assert_eq!(got, want);
}

#[test]
fn refuses_to_overwrite_input() {
    let d = temp_dir("overwrite");
    let input = d.join("in.wav");
    std::fs::write(&input, ramp_wav(100)).expect("write input");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&input)
        .args(["--delete", "..@10"])
        .output()
        .expect("run lac");
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&input).expect("input"), ramp_wav(100));
}

#[test]
fn info_reports_length() {
    let d = temp_dir("info");
    let input = d.join("in.wav");
    std::fs::write(&input, ramp_wav(1500)).expect("write input");
    let out = lac().arg("info").arg(&input).output().expect("run lac");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("0:01.500"), "{text}");
    assert!(text.contains("channels:    1"), "{text}");
}

#[test]
fn rejects_unsupported_format() {
    let d = temp_dir("format");
    let input = d.join("in.mp3");
    std::fs::write(&input, b"not really").expect("write input");
    let out = lac().arg("info").arg(&input).output().expect("run lac");
    assert!(!out.status.success());
}

#[test]
fn flac_is_detected_and_cut() {
    let d = temp_dir("flac");
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures/flac_stereo16_44k.flac");
    let output = d.join("out.flac");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--delete", "0.25..1"])
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info = lac().arg("info").arg(&output).output().expect("run lac");
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(text.contains("FLAC"), "{text}");
    // 1.5 s source minus 0.75 s.
    assert!(text.contains("0:00.750"), "{text}");
    assert!(!text.contains("warning"), "{text}");
}

#[test]
fn tags_are_set_and_shown() {
    let d = temp_dir("tags");
    // TITLE=Fixture, ARTIST=losslessaudiocut.
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures/flac_wasted_22k.flac");
    let output = d.join("out.flac");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args([
            "--keep",
            "0.25..",
            "--tag",
            "title=Part 2",
            "--tag",
            "TRACK=2",
        ])
        .args(["--tag", "artist="])
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info = lac().arg("info").arg(&output).output().expect("run lac");
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(text.contains("title:       Part 2"), "{text}");
    assert!(text.contains("track:       2"), "{text}");
    assert!(!text.contains("artist:"), "{text}");

    for bad in ["nonsense=1", "track=two", "title"] {
        let out = lac()
            .arg("cut")
            .arg(&input)
            .arg("-o")
            .arg(d.join("bad.flac"))
            .args(["--tag", bad])
            .output()
            .expect("run lac");
        assert!(!out.status.success(), "{bad} is refused");
    }
}

#[test]
fn ogg_is_detected_and_cut() {
    let d = temp_dir("ogg");
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures/ogg_speex_16k.spx");
    let output = d.join("out.spx");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--delete", "0.2..0.6"])
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info = lac().arg("info").arg(&output).output().expect("run lac");
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(text.contains("Ogg (Speex)"), "{text}");
    // 1 s source minus 0.4 s; Speex packets are 20 ms, so the cut is exact.
    assert!(text.contains("0:00.600"), "{text}");
    assert!(!text.contains("warning"), "{text}");
}

#[test]
fn mp3_is_detected_and_cut_sample_exactly() {
    let d = temp_dir("mp3");
    let input = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../lac-container/tests/fixtures/mp3_ffmpeg_id3v2.mp3");
    let output = d.join("out.mp3");
    let out = lac()
        .arg("cut")
        .arg(&input)
        .arg("-o")
        .arg(&output)
        .args(["--keep", "0.1..0.6"])
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("(exact)"));
    let info = lac().arg("info").arg(&output).output().expect("run lac");
    let text = String::from_utf8_lossy(&info.stdout);
    assert!(text.contains("MP3"), "{text}");
    assert!(text.contains("0:00.500"), "{text}");
    assert!(!text.contains("warning"), "{text}");
}

/// 16-bit mono WAV at 8 kHz from samples in `-1..1`.
fn wav_8k(samples: &[f32]) -> Vec<u8> {
    let data: Vec<u8> = samples
        .iter()
        .flat_map(|&x| ((x * 32767.0).round() as i16).to_le_bytes())
        .collect();
    let mut f = b"RIFF".to_vec();
    f.extend((36 + data.len() as u32).to_le_bytes());
    f.extend(b"WAVEfmt ");
    f.extend(16u32.to_le_bytes());
    f.extend(1u16.to_le_bytes()); // PCM
    f.extend(1u16.to_le_bytes()); // mono
    f.extend(8000u32.to_le_bytes());
    f.extend(16000u32.to_le_bytes()); // byte rate
    f.extend(2u16.to_le_bytes()); // block align
    f.extend(16u16.to_le_bytes()); // bits
    f.extend(b"data");
    f.extend((data.len() as u32).to_le_bytes());
    f.extend(data);
    f
}

#[test]
fn detect_lists_pauses_and_track_starts() {
    let d = temp_dir("detect");
    let input = d.join("in.wav");
    // 4 s of a harmonic tone, 3 s of silence, 4 s of another tone.
    let tone = |f0: f32, secs: usize| {
        (0..secs * 8000).map(move |i| {
            let t = i as f32 / 8000.0;
            (1..=5)
                .map(|k| (std::f32::consts::TAU * f0 * k as f32 * t).sin() / k as f32)
                .sum::<f32>()
                * 0.2
        })
    };
    let s: Vec<f32> = tone(220.0, 4)
        .chain(std::iter::repeat_n(0.0, 3 * 8000))
        .chain(tone(330.0, 4))
        .collect();
    std::fs::write(&input, wav_8k(&s)).expect("write input");
    let out = lac().arg("detect").arg(&input).output().expect("run lac");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let pauses: Vec<&str> = stdout.lines().filter(|l| l.starts_with("pause ")).collect();
    let tracks: Vec<&str> = stdout.lines().filter(|l| l.starts_with("track ")).collect();
    assert_eq!(pauses.len(), 1, "{stdout}");
    assert_eq!(tracks.len(), 1, "{stdout}");
    // The pause is 4 s .. 7 s within one 32 ms analysis frame, the new
    // track starts 0.25 s before the second tone.
    assert!(pauses[0].starts_with("pause 0:04.0"), "{stdout}");
    assert!(
        pauses[0].contains(" .. 0:07.0") || pauses[0].contains(" .. 0:06.9"),
        "{stdout}"
    );
    assert!(tracks[0].contains("after a pause of"), "{stdout}");
    let _ = std::fs::remove_dir_all(&d);
}

/// Runs an external tool, or returns `None` if it is not installed (fatal
/// when `LAC_REQUIRE_TOOLS` is set, as in the Linux CI job).
fn run_tool(cmd: &mut Command) -> Option<std::process::Output> {
    match cmd.output() {
        Ok(o) => Some(o),
        Err(_) if std::env::var_os("LAC_REQUIRE_TOOLS").is_some() => {
            panic!("{cmd:?}: tool required (LAC_REQUIRE_TOOLS is set) but not installed")
        }
        Err(_) => {
            eprintln!("{cmd:?}: tool not installed, skipping");
            None
        }
    }
}

/// The output of `lac completions SHELL [extra args]`.
fn completion_script(shell: &str, extra: &[&str]) -> String {
    let out = lac()
        .args(["completions", shell])
        .args(extra)
        .output()
        .expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("UTF-8 script")
}

/// Asks the generated bash completion what it offers for `line` (the
/// command line typed so far, completing its last word), the way bash
/// calls a completion function on Tab.
#[cfg(unix)]
fn bash_complete(script: &str, line: &str) -> Option<Vec<String>> {
    let d = temp_dir(&format!("bash-{}", line.len()));
    let path = d.join("completion.bash");
    std::fs::write(&path, script).expect("write script");
    let words: Vec<&str> = line.split(' ').collect();
    let quoted: Vec<String> = words.iter().map(|w| format!("'{w}'")).collect();
    let program = format!(
        "source '{}'\n\
         COMP_WORDS=({})\n\
         COMP_CWORD={}\n\
         COMP_LINE='{line}'\n\
         COMP_POINT={}\n\
         func=$(complete -p '{}' | sed -n 's/.*-F \\([^ ]*\\).*/\\1/p')\n\
         \"$func\" '{}' '{}' '{}'\n\
         printf '%s\\n' \"${{COMPREPLY[@]}}\"",
        path.display(),
        quoted.join(" "),
        words.len() - 1,
        line.len(),
        words[0],
        words[0],
        words[words.len() - 1],
        words[words.len().saturating_sub(2)],
    );
    let out = run_tool(Command::new("bash").args(["--norc", "-c", &program]))?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

#[cfg(unix)]
#[test]
fn bash_completion_offers_subcommands_and_options() {
    let script = completion_script("bash", &[]);
    let Some(subs) = bash_complete(&script, "lac c") else {
        return;
    };
    assert!(subs.contains(&"cut".to_string()), "{subs:?}");
    assert!(subs.contains(&"completions".to_string()), "{subs:?}");
    // The hidden packager command is not offered.
    assert!(!subs.iter().any(|s| s == "manpages"), "{subs:?}");

    let opts = bash_complete(&script, "lac cut in.wav --d").expect("bash ran before");
    assert_eq!(opts, ["--delete"]);
    let shells = bash_complete(&script, "lac completions f").expect("bash ran before");
    assert_eq!(shells, ["fish"]);
}

#[cfg(unix)]
#[test]
fn bash_completion_works_under_another_name() {
    // The snap calls the CLI `losslessaudiocut.cli`.
    let script = completion_script("bash", &["--bin-name", "losslessaudiocut.cli"]);
    let Some(subs) = bash_complete(&script, "losslessaudiocut.cli de") else {
        return;
    };
    assert_eq!(subs, ["detect"]);
}

#[test]
fn completions_exist_for_other_shells() {
    for (shell, marker) in [
        ("zsh", "#compdef lac"),
        ("fish", "complete -c lac"),
        ("powershell", "Register-ArgumentCompleter"),
        ("elvish", "edit:completion:arg-completer[lac]"),
    ] {
        let script = completion_script(shell, &[]);
        assert!(script.contains(marker), "{shell}: {marker:?} missing");
        // fish names long options without dashes (`-l delete`).
        assert!(script.contains("delete"), "{shell}: options missing");
    }
}

#[test]
fn manpages_cover_every_visible_command() {
    let d = temp_dir("man");
    let out = lac().arg("manpages").arg(&d).output().expect("run lac");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut names: Vec<String> = std::fs::read_dir(&d)
        .expect("read dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "lac-completions.1",
            "lac-cut.1",
            "lac-detect.1",
            "lac-info.1",
            "lac.1"
        ]
    );
    let main = std::fs::read_to_string(d.join("lac.1")).expect("lac.1");
    assert!(main.contains(".TH lac 1"), "title");
    assert!(main.contains(".SH EXAMPLES"));
    assert!(main.contains("lac\\-cut(1)"), "SEE ALSO and SUBCOMMANDS");
    assert!(!main.contains("manpages"), "hidden command leaked");
    let cut = std::fs::read_to_string(d.join("lac-cut.1")).expect("lac-cut.1");
    assert!(cut.contains("\\-\\-delete"));

    // Every page must format without groff warnings.
    for name in &names {
        let Some(o) = run_tool(
            Command::new("groff")
                .args(["-man", "-Tutf8", "-ww", "-z"])
                .arg(d.join(name)),
        ) else {
            return;
        };
        assert!(
            o.status.success() && o.stderr.is_empty(),
            "{name}: {}",
            String::from_utf8_lossy(&o.stderr)
        );
    }
}
