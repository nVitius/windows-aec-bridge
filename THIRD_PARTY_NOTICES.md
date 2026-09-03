# Third-party notices

This prototype depends on open-source Rust crates recorded in `Cargo.lock`. The principal audio
components are:

- [Sonora 0.2.0](https://github.com/dignifiedquire/sonora), BSD-3-Clause, a pure-Rust port of the
  WebRTC audio-processing modules used here for AEC3.
- [wasapi-rs 0.24.0](https://github.com/HEnquist/wasapi-rs), MIT, used for direct Windows audio I/O.
- [CPAL 0.18.2](https://github.com/RustAudio/cpal), Apache-2.0 OR MIT, used only for endpoint
  discovery and format reporting.
- [ringbuf 0.5](https://github.com/agerasev/ringbuf), MIT, used for lock-free audio transfer.
- [eframe/egui 0.36.1](https://github.com/emilk/egui), Apache-2.0 OR MIT, used for the
  Windows control panel and persisted application settings.
- [tray-icon 0.24.2](https://github.com/tauri-apps/tray-icon), Apache-2.0 OR MIT, used for the
  Windows notification-area icon and lifecycle.
- [Muda 0.19.3](https://github.com/tauri-apps/muda), Apache-2.0 OR MIT, used by `tray-icon` for the
  notification-area menu.
- [serde 1](https://github.com/serde-rs/serde), Apache-2.0 OR MIT, used to serialize GUI settings.
- [windows 0.62.2](https://github.com/microsoft/windows-rs), Apache-2.0 OR MIT, used for
  per-user Windows startup registration, single-instance coordination, and native messages.

The complete transitive dependency versions are preserved in `Cargo.lock`. The release package's
`THIRD_PARTY_LICENSES.html` contains the corresponding license texts and attribution generated for
the Windows x64 dependency graph. No virtual-audio driver or driver license is bundled with this
application.
