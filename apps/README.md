# Apps

Every Flicks app lives here, beside the Rust crates in `../crates`.

| Path | App | Build system |
|---|---|---|
| `desktop/` | Flicks for macOS, Windows and Linux (Rune): library and player | Cargo, package `flicks-desktop` |
| `admin/` | Flicks Server's admin window (Rune) | Cargo, package `flicks-admin` |
| `apple/` | Flicks for iOS, iPadOS and tvOS (SwiftUI shells on the Rust client core and player) | Xcode project; links `flicks-ffi` as an XCFramework |
| `android/` | Flicks for Android phones, Android TV and Fire TV (Compose shells on the same core) | Gradle; loads `flicks-ffi` through JNI |
| `web/` | Later | — |

The Rust apps join the Cargo workspace by path when they're created (add `"apps/desktop"` or `"apps/admin"` to `members` in the root `Cargo.toml`). `apple/` and `android/` are never workspace members: they build the Rust side through `crates/ffi`.

Apps stay thin: windows, input and drawing. Anything two apps need goes in a crate (`client`, `player`, `ffi`).
