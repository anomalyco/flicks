//! Headless snapshots of Home, a title's page, search and the Server screens, rendered by the app binary with Rune's
//! `--snap` (the same renderer as the window) and checked against its `--tree`: the texts are on
//! screen, and the posters and Play button are drawn where the tree puts them.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

const SCALE: f32 = 2.0;

fn render(scene: &str, flag: &str, out: &Path) {
    let scale = SCALE.to_string();
    let run = Command::new(env!("CARGO_BIN_EXE_flicks-desktop"))
        .args([flag, &out.to_string_lossy(), "--scene", scene, "--scale", &scale])
        .output()
        .unwrap();
    assert!(run.status.success(), "{scene}: {}", String::from_utf8_lossy(&run.stderr));
}

fn dir() -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn rect(element: &Value) -> [f32; 4] {
    let r: Vec<f32> = element["rect"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect();
    [r[0], r[1], r[2], r[3]]
}

#[test]
fn library_and_server_screens_render_headless() {
    // Each scene: texts it shows, and keyed elements (by prefix) whose box must be drawn.
    let scenes: [(&str, &[&str], &[&str]); 8] = [
        // The featured title over Home's first row.
        (
            "home",
            &["VisionQuest", "Resume", "More Info", "Continue Watching", "S1 · E2  ·  31m left"],
            &["title-", "play"],
        ),
        (
            "detail",
            &["VisionQuest", "2026  ·  2 episodes  ·  4K  ·  HDR", "Resume  S1 · E2", "Season 1", "Episode 2"],
            &["play"],
        ),
        ("search", &["S1 · E2", "S1 · E6"], &["title-"]),
        ("servers", &["Servers", "Living Room Mac", "Online", "Admin", "Ana's PC", "Offline", "Viewer"], &["server-"]),
        (
            "server-folders",
            &["Living Room Mac", "Folders", "/Volumes/Media/TV", "Remove", "Add Folder"],
            &["folder-field"],
        ),
        ("server-users", &["admin", "ana", "Viewer", "Add User"], &["user-field"]),
        ("server-devices", &["Flicks for Mac", "This device", "Ana's iPhone", "Remove"], &[]),
        // The QR code (of a dummy link) and Copy Link are drawn; the link itself is never shown.
        ("server-pairing", &["Open Pairing Window", "Open for 5 minutes, for one device."], &["qr", "copy-link"]),
    ];
    let dir = dir();
    for (scene, texts, drawn) in scenes {
        let png = dir.join(format!("{scene}.png"));
        render(scene, "--snap", &png);
        let image = flicks_testkit::golden::read_png(&png).unwrap();
        assert_eq!((image.w, image.h), (2360, 1520), "{scene}: the 1180×760 window at 2×");

        let json = dir.join(format!("{scene}.json"));
        render(scene, "--tree", &json);
        let tree: Value = serde_json::from_slice(&std::fs::read(&json).unwrap()).unwrap();
        let elements = tree["elements"].as_array().unwrap();
        let on_screen: Vec<&Value> = elements.iter().filter(|e| rect(e)[1] < 760.0).collect();
        for text in texts {
            assert!(on_screen.iter().any(|e| e["text"] == *text), "{scene} shows {text:?}");
        }

        // A point just inside each element differs from the page behind it.
        let background = image.get(4, image.h - 4);
        for prefix in drawn {
            let element = on_screen
                .iter()
                .find(|e| e["key"].as_str().is_some_and(|k| k.starts_with(prefix)))
                .unwrap_or_else(|| panic!("{scene}: no {prefix} element"));
            let [x, y, ..] = rect(element);
            let pixel = image.get(((x + 6.0) * SCALE) as u32, ((y + 30.0) * SCALE) as u32);
            let distance = (0..3).map(|c| pixel[c].abs_diff(background[c]) as u32).sum::<u32>();
            assert!(distance > 30, "{scene}: {prefix} isn't drawn ({pixel:?} on {background:?})");
        }
    }
}
