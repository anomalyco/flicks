# Apps

Every Flicks app lives here, beside the Rust crates in `../crates`.

| Path | App | Build system |
|---|---|---|
| `desktop/` | Flicks for macOS, then Linux and Windows (Rune): library, player, and Server screens for admins | Cargo, package `flicks-desktop` |
| `apple/` | Later: Flicks for iOS, iPadOS and tvOS (SwiftUI shells on the Rust client core and player) | Xcode project; links `flicks-ffi` as an XCFramework |
| `android/` | Later: Flicks for Android phones, Android TV and Fire TV (Compose shells on the same core) | Gradle; loads `flicks-ffi` through JNI |
| `web/` | Flicks in the browser: the same screens (`crates/ui`) on Rune's web host, served by the server at `/` (`flicks serve --web`) | Cargo, package `flicks-web`; `web/build.sh` makes the folder to serve |

The desktop app joins the Cargo workspace by path when it's created (add `"apps/desktop"` to `members` in the root `Cargo.toml`). `apple/` and `android/` are never workspace members: they build the Rust side through `crates/ffi`.

Apps stay thin: windows, input and drawing. Anything two apps need goes in a crate (`client`, `player`, `ffi`).
