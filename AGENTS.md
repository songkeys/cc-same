# Project Working Agreement

## Version every update

- Every update delivered to the user must increment the patch version by at least one (for example, `0.1.12` → `0.1.13`), including local builds installed in Applications. Do not replace an installed app with changed code under the same version number.
- Keep the workspace version in `Cargo.toml`, the desktop version in `crates/app/Cargo.toml`, and their lockfiles in sync. Add the matching entry to `CHANGELOG.md`.
- Before installation, compare the new version with the installed app. After installation, verify the installed app's version and running executable path.
- For changes affecting the menu bar or windows, verify the actual left-click, right-click, and window behavior; a successful build or running process alone is not UI verification. Explicitly report anything that could not be verified.
