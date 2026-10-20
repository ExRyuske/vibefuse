#!/bin/sh
# Builds the release package for this system into target/dist, with the
# slower, fully optimized `dist` profile:
#   macOS    VibeFuse.app and VibeFuse-<version>-macos.dmg. UNIVERSAL=1 builds
#            for Apple silicon and Intel in one app (needs `rustup target add
#            aarch64-apple-darwin x86_64-apple-darwin`).
#   Windows  VibeFuse-<version>-windows-x64.exe, the program itself, and
#            THIRD-PARTY-NOTICES.txt beside it; in Git Bash. Elsewhere
#            `sh scripts/package.sh windows` cross-builds with mingw-w64.
#   Linux    VibeFuse-<version>-linux-<arch>.tar.gz and, where rpmbuild is
#            installed, the .rpm for Fedora. Needs build-essential pkg-config
#            libfontconfig1-dev libfreetype-dev libxkbcommon-dev (on Fedora gcc
#            fontconfig-devel freetype-devel libxkbcommon-devel).
# The packages carry THIRD-PARTY-NOTICES.txt, which takes Python 3.
set -eu

cd "$(dirname "$0")/.."
# Release binaries carry no paths of the build machine (they would name its
# user in panic messages).
home=$HOME
here=$(pwd)
if command -v cygpath >/dev/null 2>&1; then
	# Git Bash on Windows: the compiler sees Windows paths.
	home=$(cygpath -w "$HOME")
	here=$(cygpath -w "$here")
fi
RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$home/.cargo=/cargo --remap-path-prefix=$home/.rustup=/rustup --remap-path-prefix=$here=/vibefuse"
export RUSTFLAGS
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
out=target/dist
# Windows may have a python3 that only opens the Microsoft Store.
python=
for candidate in python3 python; do
	if "$candidate" -c "" >/dev/null 2>&1; then
		python=$candidate
		break
	fi
done
if [ -z "$python" ]; then
	echo "Python 3 is needed for THIRD-PARTY-NOTICES.txt" >&2
	exit 1
fi

notices() {
	"$python" scripts/third-party.py "$1/THIRD-PARTY-NOTICES.txt" >/dev/null
}

macos() {
	binary=$out/vibefuse
	if [ "${UNIVERSAL:-}" = 1 ]; then
		# One cargo run builds both at once.
		cargo build --profile dist --locked --target aarch64-apple-darwin --target x86_64-apple-darwin
		binary=$out/vibefuse-universal
		lipo -create -output "$binary" \
			target/aarch64-apple-darwin/dist/vibefuse \
			target/x86_64-apple-darwin/dist/vibefuse
	else
		cargo build --profile dist --locked
	fi

	app=$out/VibeFuse.app
	rm -rf "$app"
	mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/en.lproj" "$app/Contents/Resources/ru.lproj"
	cp "$binary" "$app/Contents/MacOS/vibefuse"
	cp LICENSE "$app/Contents/Resources/"
	notices "$app/Contents/Resources"
	# The microphone prompt in Russian; English is in Info.plist.
	cat >"$app/Contents/Resources/ru.lproj/InfoPlist.strings" <<EOF
"NSMicrophoneUsageDescription" = "VibeFuse подаёт вход MiniFuse в свои миксы и выводит микс Monitor & Phones на выходы карты.";
EOF

	iconset=$out/AppIcon.iconset
	rm -rf "$iconset"
	mkdir -p "$iconset"
	for size in 16 32 128 256 512; do
		sips -z $size $size assets/icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
		sips -z $((size * 2)) $((size * 2)) assets/icon-1024.png --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
	done
	iconutil -c icns -o "$app/Contents/Resources/AppIcon.icns" "$iconset"
	rm -rf "$iconset"

	cat >"$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key>
	<string>VibeFuse</string>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleLocalizations</key>
	<array>
		<string>en</string>
		<string>ru</string>
	</array>
	<key>CFBundleDisplayName</key>
	<string>VibeFuse</string>
	<key>CFBundleIdentifier</key>
	<string>cc.yeru.vibefuse</string>
	<key>CFBundleExecutable</key>
	<string>vibefuse</string>
	<key>CFBundleIconFile</key>
	<string>AppIcon</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
	<key>LSMinimumSystemVersion</key>
	<string>11.0</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.music</string>
	<key>NSHighResolutionCapable</key>
	<true/>
	<key>NSMicrophoneUsageDescription</key>
	<string>VibeFuse feeds the MiniFuse input into its mixes and plays the Monitor &amp; Phones mix on the MiniFuse outputs.</string>
	<key>NSHumanReadableCopyright</key>
	<string>GPL-3.0-or-later. Not affiliated with Arturia.</string>
</dict>
</plist>
EOF

	# An ad-hoc signature gives the app a stable identity for the microphone
	# permission. Distributing without Gatekeeper warnings needs a Developer
	# ID signature and notarization instead.
	codesign --force --sign - "$app"

	dmg=$out/VibeFuse-$version-macos.dmg
	staging=$out/dmg
	rm -rf "$staging" "$dmg"
	mkdir -p "$staging"
	cp -R "$app" "$staging/"
	ln -s /Applications "$staging/Applications"
	hdiutil create -quiet -volname VibeFuse -srcfolder "$staging" -format UDZO "$dmg"
	rm -rf "$staging"
	echo "$dmg"
}

windows() {
	case "$(uname -s)" in
	MINGW* | MSYS* | CYGWIN*) target=${TARGET:-x86_64-pc-windows-msvc} ;;
	*) target=${TARGET:-x86_64-pc-windows-gnu} ;;
	esac
	# The program is downloaded on its own, so it carries the C runtime
	# instead of needing the Visual C++ redistributable. (MinGW builds use
	# only what comes with Windows anyway.)
	case "$target" in
	*-msvc)
		RUSTFLAGS="$RUSTFLAGS -C target-feature=+crt-static"
		export RUSTFLAGS
		;;
	esac
	cargo build --profile dist --locked --target "$target"

	exe=$out/VibeFuse-$version-windows-x64.exe
	cp "target/$target/dist/vibefuse.exe" "$exe"
	notices "$out"
	echo "$exe"
}

linux() {
	cargo build --profile dist --locked

	name=VibeFuse-$version-linux-$(uname -m)
	staging=$out/$name
	rm -rf "$staging"
	mkdir -p "$staging"
	cp "$out/vibefuse" packaging/linux/install.sh packaging/linux/vibefuse.desktop \
		packaging/linux/70-vibefuse.rules README.md README.ru.md LICENSE "$staging/"
	cp assets/icon-256.png "$staging/vibefuse.png"
	notices "$staging"
	tar -C "$out" -czf "$out/$name.tar.gz" "$name"
	echo "$out/$name.tar.gz"
	if command -v rpmbuild >/dev/null 2>&1; then
		rpmbuild -bb --quiet --define "_topdir $here/$out/rpm" \
			--define "_sourcedir $here/$staging" --define "version $version" \
			packaging/linux/vibefuse.spec
		mv "$out"/rpm/RPMS/*/vibefuse-*.rpm "$out/$name.rpm"
		rm -rf "$out/rpm"
		echo "$out/$name.rpm"
	fi
	rm -rf "$staging"
}

case "${1:-$(uname -s)}" in
Darwin | macos) macos ;;
MINGW* | MSYS* | CYGWIN* | windows) windows ;;
Linux | linux) linux ;;
*)
	echo "usage: sh scripts/package.sh [macos | windows | linux]" >&2
	exit 2
	;;
esac
