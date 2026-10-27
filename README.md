# VibeFuse

A control panel for the Arturia MiniFuse 1 audio interface on macOS and Windows.

[Русская версия](README.ru.md)

![VibeFuse](docs/screenshot.png)

- Input settings (INST, +48V, Direct monitor), level meters and the sample rate.
- A mixer with three mixes: Monitor & Phones, Mix 3/4 and Mix 5/6. On Windows it uses
  Arturia's driver; on macOS VibeFuse adds virtual devices for it.
- Effects on the microphone: VST3, VST2, Audio Unit (macOS) and LADSPA plug-ins.
- Settings are restored every time the interface is connected.
- Presets compatible with MiniFuse Control Center.
- Runs in the menu bar or tray. English and Russian.

## Install

Download the latest version from the Releases page.

- **macOS 11 or later.** Open the `.dmg` and drag VibeFuse to Applications. The app is not
  notarized, so open it the first time with right-click → Open.
- **Windows 10 or 11.** Install Arturia's MiniFuse Audio Driver first (it comes with
  MiniFuse Control Center). Put the `.exe` in a folder where it will stay and run it. The
  program is not signed, so Windows asks the first time: More info → Run anyway.

## Virtual outputs (macOS)

Install them in Settings → Virtual outputs. Apps then see these devices:

| Device | Use |
|---|---|
| VibeFuse Main 1/2 | main playback |
| VibeFuse Virtual 3/4, 5/6, 7/8 | playback for separate apps |
| VibeFuse Mix 3/4, Mix 5/6 | record a mix in streaming or call apps |
| VibeFuse Loopback Main 7/8 | a copy of Monitor & Phones |

This needs the administrator password and access to the microphone, through which the
interface's input reaches the mixes.

To hear yourself in headphones, turn on Input 1 in the Monitor & Phones mix, or press
DIRECT for the interface's own monitoring without delay. While Input 1 is heard there, the
system shows the microphone as in use.

## Effects on the microphone

![Effects](docs/effects.png)

Press FX on the input, then Add…. VibeFuse runs VST3, VST2, Audio Unit (macOS) and LADSPA
plug-ins on Input 1, top to bottom, and plays the result on the output you choose. The
switch beside each plug-in bypasses it, and Open shows its own window. Its settings are on
the page too, with sliders; click a value to type one, like `-6 dB` or `50%`. VibeFuse
looks for plug-ins in the usual folders; Folder… adds another one. A chain exports to a
file and imports on another computer, where a missing plug-in is looked for by name.

The effects add no buffer of their own, only the delay a plug-in reports (a limiter's
lookahead, say):

- **macOS.** The output is any device that plays sound: the MiniFuse to hear yourself, a
  VibeFuse device (Virtual 5/6, say) to have the processed voice in the mixer as a strip
  of its own, or a virtual cable to another app. The effects run in blocks of 128 samples
  (2.7 ms at 48 kHz) on the MiniFuse's clock, which another device follows.
- **Windows.** The output is a pair of outputs of Arturia's ASIO driver, at the buffer size
  set in Settings. Out 3/4 come back in the mixer as Virtual 3/4. While the effects are on,
  other programs cannot use ASIO: switch them off to free it.

Plug-ins load into VibeFuse itself, so a faulty one can take it down. One that crashes it
while being looked at is skipped until it changes.

## Build

```sh
cargo run --release
cargo test
```

macOS needs the Xcode Command Line Tools. `sh scripts/package.sh` builds the release
package for your system. `cargo test -- --ignored installed` runs every effect installed on
the computer through the chain.

Releases go out by themselves: once CI passes on `main` and the program has changed, the
Release workflow raises the patch version, tags it and publishes the macOS and Windows
packages. For a minor or major version, or to release the current one (`none`), run it by
hand in GitHub Actions.

## Privacy

VibeFuse collects nothing. While its window is open it checks
[Arturia's MiniFuse page](https://www.arturia.com/products/audio/minifuse/resources) once a
day for new firmware.

## License

GPL-3.0-or-later, see [LICENSE](LICENSE). The licenses of the components VibeFuse is
built from are in THIRD-PARTY-NOTICES.txt, in the packages and on each release page.

VibeFuse is not affiliated with Arturia. Arturia and MiniFuse are trademarks of Arturia.
VST is a trademark of Steinberg Media Technologies GmbH, Audio Units of Apple Inc.
