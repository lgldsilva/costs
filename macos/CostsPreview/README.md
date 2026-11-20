# Costs Prévia (macOS)

A compact SwiftUI/AppKit status-item popover for visually reviewing the existing `~/.local/bin/costs status` JSON. This is a standalone **prototype**; it does not replace or modify an installed menubar UI. It reads the status JSON (no credentials, transcript parsing, or SQLite access). Opening the popover runs `costs quota --if-stale` (network only for providers past the cache TTL, at most every 5 minutes); **Atualizar cotas agora** runs `costs quota --refresh`. The only write-like external operation is the explicit **Abrir relatório completo** button, which invokes `costs report --format html --open` with argument arrays.

## Build and test

Requires macOS 14+ and a local Swift 6 toolchain / macOS SDK. No package dependencies or Xcode project required.

```sh
cd macos/CostsPreview
swift test
swift build -c release
```

## Create a local app bundle (does not install or launch it)

```sh
cd macos/CostsPreview
swift build -c release
APP=/tmp/CostsPreview.app
mkdir -p "$APP/Contents/MacOS"
cp .build/release/CostsPreview "$APP/Contents/MacOS/CostsPreview"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple Computer//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>CostsPreview</string>
<key>CFBundleIdentifier</key><string>local.costs.preview</string>
<key>CFBundleName</key><string>Costs Prévia</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
<key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
```

To launch later for visual review (not run as part of build/test):

```sh
open /tmp/CostsPreview.app --args --open
# Or an offline fixture, with appearance override:
open /tmp/CostsPreview.app --args --snapshot /path/to/status.json --appearance dark --open
```

Supported flags: `--snapshot PATH` reads that JSON file only and disables refresh/report actions; `--appearance light|dark` applies a local visual override; `--open` shows the popover on launch. Without a fixture the first popover opening asynchronously reads `costs status`, a read-only cache-backed command.

## Data limitations

The optional provider `plan` is displayed only if the status producer supplies a nonempty string; older JSON remains valid. A detected plan label and configured monthly subscription are distinct fields. Missing/null amounts render as unavailable rather than zero. Provider window labels and Rust-generated pace hints are kept as supplied. Aggregate collection age/staleness is displayed separately from provider quota freshness. Monthly API-equivalent cost is an estimate, not a bill and not net of subscriptions. Per-machine/tool cost and token figures come only from the published `agents` status rows. Historical period views remain in the existing HTML report. The UI does not fabricate daily/30-day analytics, account identity, quota plan, or unreported data. There is no background polling.
