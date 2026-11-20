import Foundation

protocol CommandRunning: Sendable {
    func run(executableURL: URL, arguments: [String], timeout: Duration) async throws -> Data
}

struct ProcessCommandRunner: CommandRunning {
    func run(executableURL: URL, arguments: [String], timeout: Duration) async throws -> Data {
        let process = Process()
        process.executableURL = executableURL
        process.arguments = arguments
        process.standardInput = FileHandle.nullDevice
        let output = Pipe()
        let errors = Pipe()
        process.standardOutput = output
        process.standardError = errors
        try process.run()

        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Data, Error>) in
                let gate = ProcessGate()
                let watchdogBox = WatchdogBox()
                let watchdog = DispatchWorkItem {
                    guard gate.finish() else { return }
                    if process.isRunning { process.terminate() }
                    continuation.resume(throwing: LoaderError.timeout)
                }
                watchdogBox.set(watchdog)
                let components = timeout.components
                let timeoutSeconds = max(0, Double(components.seconds) + Double(components.attoseconds) / 1_000_000_000_000_000_000)
                DispatchQueue.global().asyncAfter(deadline: .now() + timeoutSeconds, execute: watchdog)

                process.terminationHandler = { finishedProcess in
                    let stdout = output.fileHandleForReading.readDataToEndOfFile()
                    let stderr = errors.fileHandleForReading.readDataToEndOfFile()
                    guard gate.finish() else { return }
                    watchdogBox.cancel()
                    if finishedProcess.terminationStatus == 0 {
                        continuation.resume(returning: stdout)
                    } else {
                        let message = String(data: stderr, encoding: .utf8) ?? ""
                        continuation.resume(throwing: LoaderError.commandFailed(message.trimmingCharacters(in: .whitespacesAndNewlines)))
                    }
                }
                if Task.isCancelled {
                    watchdogBox.cancel()
                    guard gate.finish() else { return }
                    if process.isRunning { process.terminate() }
                    continuation.resume(throwing: CancellationError())
                }
            }
        } onCancel: {
            if process.isRunning { process.terminate() }
        }
    }
}

struct StatusLoader: Sendable {
    let snapshotURL: URL?
    let executableURL: URL
    let timeout: Duration
    let runner: any CommandRunning

    init(
        snapshotURL: URL?,
        executableURL: URL = URL(fileURLWithPath: NSHomeDirectory()).appending(path: ".local/bin/costs"),
        timeout: Duration = .seconds(8),
        runner: any CommandRunning = ProcessCommandRunner()
    ) {
        self.snapshotURL = snapshotURL
        self.executableURL = executableURL
        self.timeout = timeout
        self.runner = runner
    }

    func load() async throws -> StatusSnapshot {
        let data: Data
        if let snapshotURL { data = try Data(contentsOf: snapshotURL, options: .mappedIfSafe) }
        else { data = try await run(arguments: ["status"]) }
        return try JSONDecoder().decode(StatusSnapshot.self, from: data)
    }

    /// Atualiza o cache de cotas antes de reler o status. `force` (ação explícita) usa
    /// `--refresh`; a abertura do painel usa `--if-stale`, que só vai à rede para o que
    /// passou do TTL do cache. Nunca é chamado durante render.
    func refreshQuota(force: Bool) async throws {
        if snapshotURL != nil { return }
        _ = try await run(arguments: ["quota", force ? "--refresh" : "--if-stale"])
    }

    private func run(arguments: [String]) async throws -> Data {
        try await runner.run(executableURL: executableURL, arguments: arguments, timeout: timeout)
    }
}

private final class WatchdogBox: @unchecked Sendable {
    private let lock = NSLock()
    private var workItem: DispatchWorkItem?
    func set(_ item: DispatchWorkItem) { lock.lock(); defer { lock.unlock() }; workItem = item }
    func cancel() { lock.lock(); defer { lock.unlock() }; workItem?.cancel() }
}

private final class ProcessGate: @unchecked Sendable {
    private let lock = NSLock()
    private var finished = false
    func finish() -> Bool {
        lock.lock(); defer { lock.unlock() }
        guard !finished else { return false }
        finished = true
        return true
    }
}

enum LoaderError: LocalizedError {
    case timeout
    case commandFailed(String)
    var errorDescription: String? {
        switch self {
        case .timeout: "A leitura excedeu o limite de tempo configurado."
        case .commandFailed(let detail): detail.isEmpty ? "Não foi possível ler costs status." : "costs status: \(detail)"
        }
    }
}
