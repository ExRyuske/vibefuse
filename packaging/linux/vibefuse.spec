# The Fedora (and other RPM distributions) package. scripts/package.sh builds
# the program first and passes its version and files to rpmbuild.
Name:           vibefuse
Version:        %{version}
Release:        1%{?dist}
Summary:        Control panel for the Arturia MiniFuse 1 audio interface
License:        GPL-3.0-or-later
URL:            https://github.com/ExRyuske/vibefuse
# Loaded while running, so rpmbuild does not see it: keyboard input on
# Wayland and X11.
Requires:       libxkbcommon
# pw-cli, pw-link and pw-dump: the virtual outputs on PipeWire.
Recommends:     pipewire-utils

# Built with cargo beforehand and already stripped.
%global debug_package %{nil}

%description
Input settings, level meters, the sample rate and a mixer with three mixes
for the Arturia MiniFuse 1, with virtual playback and recording devices on
PipeWire. Not affiliated with Arturia.

%install
install -Dm755 %{_sourcedir}/vibefuse %{buildroot}%{_bindir}/vibefuse
install -Dm644 %{_sourcedir}/vibefuse.desktop %{buildroot}%{_datadir}/applications/vibefuse.desktop
install -Dm644 %{_sourcedir}/vibefuse.png %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/vibefuse.png
install -Dm644 %{_sourcedir}/70-vibefuse.rules %{buildroot}/usr/lib/udev/rules.d/70-vibefuse.rules
install -Dm644 %{_sourcedir}/LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dm644 %{_sourcedir}/THIRD-PARTY-NOTICES.txt %{buildroot}%{_licensedir}/%{name}/THIRD-PARTY-NOTICES.txt

%post
# A MiniFuse that is already plugged in gets the new access rights at once.
udevadm control --reload-rules >/dev/null 2>&1 || :
udevadm trigger --subsystem-match=usb --attr-match=idVendor=1c75 >/dev/null 2>&1 || :

%files
%{_bindir}/vibefuse
%{_datadir}/applications/vibefuse.desktop
%{_datadir}/icons/hicolor/256x256/apps/vibefuse.png
/usr/lib/udev/rules.d/70-vibefuse.rules
%license %{_licensedir}/%{name}/LICENSE
%license %{_licensedir}/%{name}/THIRD-PARTY-NOTICES.txt
