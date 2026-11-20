import AppKit

// Entry point: SPM executable needs an explicit main; AppDelegate is @MainActor.
MainActor.assumeIsolated {
    let app = NSApplication.shared
    let delegate = AppDelegate()
    app.delegate = delegate
    app.run()
}
