# Installing test builds

Test builds of CodeVox mU are unsigned installers. The Package workflow uploads them as GitHub Actions artifacts. It does not publish a GitHub Release and it does not push a tag.

The workflow runs on every push to `main`, and it can be started by hand: Actions, then Package, then Run workflow.

## Download

Open the workflow run and download the artifact for the machine you are on. GitHub wraps each artifact in a zip. Unzip it, then install the file inside.

| Artifact | What is inside |
| --- | --- |
| `codevox-mu-windows-x64-nsis` | NSIS installer for 64-bit Windows |
| `codevox-mu-windows-x64-zip` | `codevox-mu.exe` plus `assets/fonts` and the icon |
| `codevox-mu-windows-arm64-nsis` | NSIS installer for Windows on ARM |
| `codevox-mu-windows-arm64-zip` | Arm64 `codevox-mu.exe` plus the same assets |
| `codevox-mu-macos-universal-dmg` | Universal macOS disk image (Apple silicon and Intel) |
| `codevox-mu-linux-appimage` | AppImage |
| `codevox-mu-linux-deb` | Debian package |
| `codevox-mu-linux-rpm` | RPM package |

Artifacts expire. Run the workflow again when a download is gone.

The Windows zip is the fallback when the installer is blocked. Unzip it and run `codevox-mu.exe`. The fonts are also compiled into the binary. The copies in the package are the SIL OFL files and the faces themselves.

## Windows SmartScreen

The installer and the exe have no Authenticode signature. SmartScreen shows "Windows protected your PC" and an unknown publisher.

On the installer, choose More info, then Run anyway.

If the zip's exe will not start, open its Properties and tick Unblock, then run it again. That clears the mark of the web on the downloaded file.

## macOS Gatekeeper

The disk image is ad-hoc signed (`codesign -s -`) and not notarised. There is no paid Apple certificate. After a browser download, Gatekeeper quarantines the app.

Open the dmg and drag CodeVox mU to Applications. The first launch has to be a right-click: control-click the app, choose Open, then Open again.

If macOS says the app is damaged or from an unidentified developer, clear the quarantine flag and open it again:

```sh
xattr -dr com.apple.quarantine "/Applications/CodeVox mU.app"
```

## Linux

The AppImage uses the system Vulkan loader and the system X11 or Wayland libraries. Install those the same way a local build does (`libvulkan1` and the iced libraries in the README). Then:

```sh
chmod +x codevox-mu_*.AppImage
./codevox-mu_*.AppImage
```

If the file fails because FUSE or `libfuse.so.2` is missing, install `libfuse2t64` (or `libfuse2` on older Debian), or skip FUSE:

```sh
APPIMAGE_EXTRACT_AND_RUN=1 ./codevox-mu_*.AppImage
```

The deb and rpm declare the same graphics libraries, so the package manager installs them:

```sh
sudo apt install ./codevox-mu_*_amd64.deb
sudo dnf install ./codevox-mu-*.rpm
```

Fonts land in `/usr/lib/codevox-mu/fonts`. The desktop icon is `codevox-mu`.
