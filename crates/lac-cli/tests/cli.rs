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
