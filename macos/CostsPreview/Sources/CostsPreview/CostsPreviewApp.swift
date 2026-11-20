import AppKit
import SwiftUI
import ServiceManagement

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate {
    private var statusItem: NSStatusItem!
    private var processLock: ProcessLifetimeLock?
    private let popover = NSPopover()
    private let model: PreviewModel
    private let autoOpen: Bool
    private let loginService: LoginServiceControlling
    private var launchServiceError: String?
    private var launchServiceMessage: String?

    override init() {
        let args = Array(CommandLine.arguments.dropFirst())
        var snapshotURL: URL?
        var appearance: String?
        var wantsAutoOpen = false
        var initialProviderID: String?
        var index = 0
        while index < args.count {
            switch args[index] {
            case "--snapshot" where index + 1 < args.count:
                index += 1; snapshotURL = URL(fileURLWithPath: args[index])
            case "--appearance" where index + 1 < args.count:
                index += 1; appearance = args[index]
            case "--open": wantsAutoOpen = true
            case "--provider" where index + 1 < args.count:
                index += 1; initialProviderID = args[index]
            default: break
            }
            index += 1
        }
        if let appearance, ["light", "dark"].contains(appearance) {
            setenv("COSTS_PREVIEW_APPEARANCE", appearance, 1)
        }
        autoOpen = wantsAutoOpen
        model = PreviewModel(loader: StatusLoader(snapshotURL: snapshotURL), isFixture: snapshotURL != nil, initialProviderID: initialProviderID)
        loginService = MainAppLoginService()
        super.init()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        if CommandLine.arguments.contains("--register-login-item") {
            registerLoginItemAndAcknowledge()
            NSApp.terminate(nil)
            return
        }
        guard let lock = Self.claimSingleInstance() else {
            NSApp.terminate(nil)
            return
        }
        processLock = lock
        NSApp.setActivationPolicy(.accessory)
        statusItem = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        if let button = statusItem.button {
            button.toolTip = "Costs Prévia · abrir resumo de custos"
            button.setAccessibilityLabel("Abrir Costs Prévia")
            button.setAccessibilityIdentifier("costs-preview-status-item")
            button.action = #selector(statusItemClicked(_:))
            button.target = self
            button.sendAction(on: [.leftMouseUp, .rightMouseUp])
        }
        popover.behavior = .transient
        popover.animates = false
        let host = NSHostingController(rootView: PreviewPanel().environment(model))
        if #available(macOS 14.0, *) { host.sizingOptions = .preferredContentSize }
        popover.contentViewController = host
        popover.contentViewController?.view.setAccessibilityIdentifier("costs-preview-popover")
        model.onSummaryChange = { [weak self] in self?.updateStatusTitle() }
        updateStatusTitle()
        if autoOpen {
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.35) { [weak self] in self?.showPopover() }
        }
    }

    private static func claimSingleInstance() -> ProcessLifetimeLock? {
        let support = FileManager.default.homeDirectoryForCurrentUser
            .appending(path: "Library/Application Support/CostsPreview", directoryHint: .isDirectory)
        let lockPath = support.appending(path: "instance.lock")
        return ProcessLifetimeLock(lockURL: lockPath)
    }

    private func registerLoginItemAndAcknowledge() {
        let support = FileManager.default.homeDirectoryForCurrentUser
            .appending(path: "Library/Application Support/CostsPreview", directoryHint: .isDirectory)
        let lockURL = support.appending(path: "instance.lock")
        do {
            try ProcessLifetimeLock.withRegistrationClaim(lockURL: lockURL) {
                try loginService.register()
                if SMAppService.mainApp.status == .requiresApproval {
                    Self.acknowledgeRegistration("requiresApproval")
                } else if SMAppService.mainApp.status == .enabled {
                    Self.acknowledgeRegistration("registered")
                } else {
                    Self.acknowledgeRegistration("failed")
                }
            }
        } catch {
            Self.acknowledgeRegistration("failed")
        }
    }

    private static func acknowledgeRegistration(_ result: String) {
        guard let path = ProcessInfo.processInfo.environment["COSTS_PREVIEW_REGISTRATION_ACK"] else { return }
        let url = URL(fileURLWithPath: path)
        try? Data(result.utf8).write(to: url, options: .atomic)
    }

    /// Ícone-gauge: até um anel para cada janela de quota distinta.
    private static var gaugeCache: [String: NSImage] = [:]
    private func gaugeImage(for provider: Provider?) -> NSImage {
        let id = provider?.id ?? ""
        let rings = GaugeRing.select(from: provider?.windows ?? [])
        let key = id + rings.map { "-\($0.kind.rawValue):\($0.label):\(Int($0.usedPercent.rounded())):\($0.color ?? "g")" }.joined()
        if let cached = Self.gaugeCache[key] { return cached }

        let size = NSSize(width: 22, height: 22)
        let image = NSImage(size: size)
        image.lockFocus()
        let rgb = ProviderCatalog.names.first { $0.id == id }?.color ?? ProviderCatalog.RGBColor(red: 0.5, green: 0.5, blue: 0.5)
        let base = NSColor(red: rgb.red, green: rgb.green, blue: rgb.blue, alpha: 1)
        func strokeColor(_ state: String?) -> NSColor {
            switch state {
            case "red": .systemRed
            case "orange": .systemOrange
            case "green": .systemGreen
            default: base
            }
        }
        func drawRing(radius: CGFloat, width: CGFloat, used: Int, color: NSColor) {
            let track = NSBezierPath()
            track.appendArc(withCenter: NSPoint(x: 11, y: 11), radius: radius * 22 / 30, startAngle: 0, endAngle: 360)
            track.lineWidth = width
            NSColor.separatorColor.setStroke()
            track.stroke()
            let fraction = min(max(Double(used), 0), 100) / 100
            guard fraction > 0.02 else { return }
            let arc = NSBezierPath()
            arc.appendArc(withCenter: NSPoint(x: 11, y: 11), radius: radius * 22 / 30,
                          startAngle: 90, endAngle: 90 - 360 * fraction, clockwise: true)
            arc.lineWidth = width
            color.setStroke()
            arc.stroke()
        }
        let radii: [CGFloat] = [12, 9, 6]
        let widths: [CGFloat] = [2.8, 2.2, 2]
        for (index, ring) in rings.prefix(radii.count).enumerated() {
            drawRing(radius: radii[index], width: widths[index],
                     used: Int(ring.usedPercent.rounded()), color: strokeColor(ring.color))
        }

        let monogram = ProviderCatalog.monogram(for: id)
        let font = NSFont.systemFont(ofSize: monogram.count > 1 ? 6 : 7.5, weight: .bold)
        let attrs: [NSAttributedString.Key: Any] = [.font: font, .foregroundColor: NSColor.labelColor]
        let textSize = monogram.size(withAttributes: attrs)
        monogram.draw(at: NSPoint(x: (22 - textSize.width) / 2, y: (22 - textSize.height) / 2), withAttributes: attrs)
        image.unlockFocus()
        Self.gaugeCache[key] = image
        return image
    }

    private static func resetCountdown(until resetsAt: Double?) -> String? {
        guard let resetsAt, resetsAt.isFinite else { return nil }
        let seconds = resetsAt - Date().timeIntervalSince1970
        guard seconds > 0 else { return nil }
        let totalSeconds = Int(seconds)
        let hours = totalSeconds / 3600
        let minutes = (totalSeconds % 3600) / 60
        if hours >= 24 { return "\(hours / 24)d\(hours % 24)h" }
        if hours > 0 { return "\(hours)h\(String(format: "%02d", minutes))" }
        return "\(max(1, minutes))m"
    }

    private var lastStatusKey: String?

    /// Barra visual com pegada real menor, sem reduzir o popover.
    private func updateStatusTitle() {
        guard let button = statusItem.button else { return }
        let provider = model.selectedProvider
        let image = gaugeImage(for: provider)
        button.image = image
        button.image?.isTemplate = false

        let id = provider?.id ?? ""
        let windows = provider?.windows ?? []
        let key = "\(id)-" + windows.map { "\($0.label):\($0.usedPercent ?? -1):\($0.kind?.rawValue ?? "")" }.joined(separator: "|")
        guard key != lastStatusKey else { return }
        lastStatusKey = key
        button.title = ""

        let name = ProviderCatalog.name(for: id)
        let providerStatus = providerStatusText(for: provider)
        let detail = windows.map { window in
            let label = window.label.trimmingCharacters(in: .whitespaces)
            let used = window.usedPercent.map { "\(Int($0.rounded()))% usado" } ?? "n/d"
            let reset = Self.resetCountdown(until: window.resetsAt).map { " · renova \($0)" } ?? ""
            return "\(label): \(used)\(reset)"
        }.joined(separator: " · ")
        button.setAccessibilityLabel("Costs Prévia · \(name)")
        let details = [providerStatus, detail].compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
        button.toolTip = details.isEmpty ? name : "\(name) — \(details)"
    }

    private func providerStatusText(for provider: Provider?) -> String? {
        guard let provider else { return "Quota unavailable" }
        if provider.stale == true { return "Quota outdated" }
        if provider.ok == false {
            let details = [provider.errorText, provider.errorHint].compactMap { $0 }.joined(separator: " ").lowercased()
            if ["auth", "login", "session expired", "unauthorized", "401", "credencial", "sessão expirada"].contains(where: details.contains) {
                return "Authentication required"
            }
            return "Quota unavailable · request failed"
        }
        if provider.windows.allSatisfy({ $0.usedPercent == nil }) { return "Quota unavailable" }
        return nil
    }

    /// Adiciona/retira item de login somente após clique explícito no menu.
    @objc private func toggleLoginItem() {
        launchServiceError = nil
        launchServiceMessage = nil
        let paths = StartupPaths.resolve(
            homeDirectory: FileManager.default.homeDirectoryForCurrentUser,
            temporaryDirectory: URL(fileURLWithPath: NSTemporaryDirectory(), isDirectory: true)
        )
        let temporary = paths.isTemporaryApp(Bundle.main.bundleURL)
        let action = StartupDecision.action(isTemporaryApp: temporary, isCurrentlyEnabled: SMAppService.mainApp.status == .enabled)
        switch action {
        case .installAndRegister:
            installAndRegister(paths: paths)
        case .registerExisting:
            do {
                try loginService.register()
                if SMAppService.mainApp.status == .requiresApproval {
                    launchServiceMessage = "Aprovação pendente em Ajustes do Sistema › Geral › Itens de Início; ainda não está habilitado."
                } else if SMAppService.mainApp.status == .enabled {
                    launchServiceMessage = "Início automático habilitado."
                } else {
                    launchServiceError = "O macOS ainda não confirmou a habilitação do início automático."
                }
            } catch {
                launchServiceError = "Não foi possível habilitar o início automático: \(error.localizedDescription)"
            }
        case .unregister:
            do {
                try loginService.unregister()
                launchServiceMessage = "Início automático desabilitado."
            } catch {
                launchServiceError = "Não foi possível desabilitar o início automático: \(error.localizedDescription)"
            }
        }
        if let launchServiceError { model.errorMessage = launchServiceError }
        else if let launchServiceMessage { model.errorMessage = launchServiceMessage }
    }

    private func installAndRegister(paths: StartupPaths) {
        let fm = FileManager.default
        let source = Bundle.main.bundleURL
        let target = paths.installedApp
        let staging = paths.applicationsDirectory.appending(path: ".CostsPreview-\(UUID().uuidString).staging.app")
        let backup = paths.applicationsDirectory.appending(path: ".CostsPreview-\(UUID().uuidString).backup.app")
        do {
            try fm.createDirectory(at: paths.applicationsDirectory, withIntermediateDirectories: true)
            let transaction = StagedInstallTransaction(
                fileManager: fm,
                target: target,
                staging: staging,
                backup: backup
            )
            let outcome = try transaction.install({ stage in
                try fm.copyItem(at: source, to: stage)
            }, launch: { installedApp in
                let ack = paths.applicationsDirectory.appending(path: ".CostsPreview-\(UUID().uuidString).ack")
                defer { try? fm.removeItem(at: ack) }
                let helper = Process()
                helper.executableURL = installedApp.appending(path: "Contents/MacOS/CostsPreview")
                helper.arguments = ["--register-login-item"]
                var environment = ProcessInfo.processInfo.environment
                environment["COSTS_PREVIEW_REGISTRATION_ACK"] = ack.path
                helper.environment = environment
                try helper.run()
                let deadline = Date().addingTimeInterval(8)
                while Date() < deadline {
                    if let result = try? String(contentsOf: ack, encoding: .utf8) {
                        switch result {
                        case "registered": return .registered
                        case "requiresApproval": return .requiresApproval
                        default: throw RegistrationFailure(message: "O novo processo não conseguiu registrar o início automático.")
                        }
                    }
                    if !helper.isRunning {
                        throw RegistrationFailure(message: "O novo processo encerrou antes de confirmar o registro.")
                    }
                    Thread.sleep(forTimeInterval: 0.05)
                }
                throw RegistrationFailure(message: "Tempo esgotado aguardando confirmação de registro.")
            })
            if outcome == .registered { NSApp.terminate(nil) }
            else {
                model.errorMessage = "Aprovação pendente em Ajustes do Sistema › Geral › Itens de Início. A cópia anterior continua disponível; o início automático ainda não está habilitado."
            }
        } catch {
            launchServiceError = "Instalação ou registro em ~/Applications falhou; a cópia anterior foi preservada quando possível: \(error.localizedDescription)"
            model.errorMessage = launchServiceError
        }
    }

    @objc private func statusItemClicked(_ sender: Any?) {
        if NSApp.currentEvent?.type == .rightMouseUp { showContextMenu(sender) }
        else if popover.isShown { popover.performClose(sender) }
        else { showPopover() }
    }

    private func showContextMenu(_ sender: Any?) {
        let menu = NSMenu()
        let refresh = NSMenuItem(title: "Atualizar cotas agora", action: #selector(refreshQuotaNow), keyEquivalent: "r")
        refresh.target = self
        menu.addItem(refresh)
        let report = NSMenuItem(title: "Abrir relatório completo", action: #selector(openReport), keyEquivalent: "")
        report.target = self
        menu.addItem(report)
        menu.addItem(.separator())
        let login = NSMenuItem(title: "Iniciar junto com o sistema", action: #selector(toggleLoginItem), keyEquivalent: "")
        login.target = self
        login.state = SMAppService.mainApp.status == .enabled ? .on : .off
        menu.addItem(login)
        menu.addItem(.separator())
        let quit = NSMenuItem(title: "Encerrar Costs Prévia", action: #selector(quitApp), keyEquivalent: "q")
        quit.target = self
        menu.addItem(quit)
        statusItem.menu = menu
        statusItem.button?.performClick(sender)
        DispatchQueue.main.async { self.statusItem.menu = nil }
    }

    @objc private func refreshQuotaNow() { model.refresh(quota: model.isFixture ? .none : .force) }

    @objc private func openReport() {
        guard !model.isFixture else { return }
        let proc = Process()
        proc.executableURL = URL(fileURLWithPath: NSHomeDirectory()).appending(path: ".local/bin/costs")
        proc.arguments = ["report", "--format", "html", "--open"]
        do { try proc.run() }
        catch { model.errorMessage = "Não foi possível abrir o relatório: \(error.localizedDescription)" }
    }

    @objc private func quitApp() { NSApp.terminate(nil) }

    private var lastAutoQuotaRefresh: Date?

    private func showPopover() {
        guard let button = statusItem.button else { return }
        popover.show(relativeTo: button.bounds, of: button, preferredEdge: .minY)
        NSApp.activate(ignoringOtherApps: true)
        if !model.isFixture,
           lastAutoQuotaRefresh.map({ Date().timeIntervalSince($0) > 300 }) ?? true {
            lastAutoQuotaRefresh = Date()
            model.refresh(quota: .ifStale)
        }
    }
}
