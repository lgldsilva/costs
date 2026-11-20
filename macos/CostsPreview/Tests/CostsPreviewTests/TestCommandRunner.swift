import Foundation
@testable import CostsPreview

struct DelayedCommandRunner: CommandRunning {
    let delay: Duration

    func run(executableURL: URL, arguments: [String], timeout: Duration) async throws -> Data {
        try await withThrowingTaskGroup(of: Data.self) { group in
            group.addTask {
                try await Task.sleep(for: delay)
                return Data(#"{"providers":[]}"#.utf8)
            }
            group.addTask {
                try await Task.sleep(for: timeout)
                throw LoaderError.timeout
            }
            return try await group.next()!
        }
    }
}

/// Grava os argumentos de cada chamada e devolve um status vazio.
final class RecordingCommandRunner: CommandRunning, @unchecked Sendable {
    private let lock = NSLock()
    private var recorded: [[String]] = []

    var calls: [[String]] { lock.withLock { recorded } }

    func run(executableURL: URL, arguments: [String], timeout: Duration) async throws -> Data {
        lock.withLock { recorded.append(arguments) }
        return Data(#"{"providers":[]}"#.utf8)
    }
}
