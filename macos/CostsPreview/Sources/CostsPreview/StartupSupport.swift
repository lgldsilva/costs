import Foundation
import Darwin

protocol LoginServiceControlling {
    func register() throws
    func unregister() throws
}

struct StartupPaths: Equatable {
    let temporaryDirectory: URL
    let applicationsDirectory: URL
    let installedApp: URL

    static func resolve(homeDirectory: URL, temporaryDirectory: URL) -> StartupPaths {
        let canonicalTemporary = temporaryDirectory.standardizedFileURL.resolvingSymlinksInPath()
        let applications = homeDirectory.appending(path: "Applications", directoryHint: .isDirectory)
        return StartupPaths(
            temporaryDirectory: canonicalTemporary,
            applicationsDirectory: applications,
            installedApp: applications.appending(path: "CostsPreview.app", directoryHint: .isDirectory)
        )
    }

    func isTemporaryApp(_ appURL: URL) -> Bool {
        let appVariants = Self.pathVariants(of: appURL.standardizedFileURL.resolvingSymlinksInPath().path)
        return Self.pathVariants(of: temporaryDirectory.path).contains { base in
            appVariants.contains { $0 == base || $0.hasPrefix(base + "/") }
        }
    }

    /// `/private` pode estar presente ou colapsado após a canonicalização, dependendo da
    /// versão do Foundation (o novo Foundation do macOS 27 colapsa `/private/tmp` para
    /// `/tmp` e não re-expande), então comparamos cada caminho nas duas formas.
    private static func pathVariants(of path: String) -> Set<String> {
        var variants: Set<String> = [path]
        if path == "/private" || path.hasPrefix("/private/") {
            variants.insert(String(path.dropFirst("/private".count)))
        } else {
            variants.insert("/private" + path)
        }
        return variants
    }
}

final class ProcessLifetimeLock {
    private let descriptor: Int32
    private var released = false

    init?(lockURL: URL) {
        let directory = lockURL.deletingLastPathComponent()
        do {
            try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true,
                                                    attributes: [.posixPermissions: 0o700])
            try FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
        } catch {
            return nil
        }
        descriptor = open(lockURL.path, O_CREAT | O_RDWR, S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else { return nil }
        guard flock(descriptor, LOCK_EX | LOCK_NB) == 0 else {
            close(descriptor)
            return nil
        }
        _ = fchmod(descriptor, mode_t(S_IRUSR | S_IWUSR))
    }

    func release() {
        guard !released else { return }
        released = true
        flock(descriptor, LOCK_UN)
        close(descriptor)
    }

    deinit { release() }

    static func registrationClaimAvailable(lockURL: URL) -> Bool {
        guard let probe = ProcessLifetimeLock(lockURL: lockURL) else { return false }
        probe.release()
        return true
    }

    static func withRegistrationClaim<T>(lockURL: URL, _ register: () throws -> T) throws -> T {
        guard let claim = ProcessLifetimeLock(lockURL: lockURL) else {
            throw RegistrationFailure(message: "Outro processo CostsPreview está em execução.")
        }
        defer { claim.release() }
        return try withExtendedLifetime(claim, register)
    }
}

enum RegistrationOutcome: Equatable {
    case registered
    case requiresApproval
}

struct RegistrationFailure: Error, LocalizedError {
    let message: String
    var errorDescription: String? { message }
}

struct CleanupFailure: Error, LocalizedError {
    let path: String
    let underlying: Error
    var errorDescription: String? { "Install committed; cleanup failed at \(path): \(underlying.localizedDescription)" }
}

protocol InstallFileOperations {
    func fileExists(atPath path: String) -> Bool
    func moveItem(at source: URL, to destination: URL) throws
    func removeItem(at URL: URL) throws
}

extension FileManager: InstallFileOperations {}

struct StagedInstallTransaction {
    private let fileManager: InstallFileOperations
    private let target: URL
    private let staging: URL
    private let backup: URL

    init(fileManager: InstallFileOperations = FileManager.default, target: URL, staging: URL, backup: URL) {
        self.fileManager = fileManager
        self.target = target
        self.staging = staging
        self.backup = backup
    }

    func install(_ prepare: (URL) throws -> Void, launch: (URL) throws -> RegistrationOutcome) throws -> RegistrationOutcome {
        var movedOldTarget = false
        do {
            try prepare(staging)
            if fileManager.fileExists(atPath: target.path) {
                try fileManager.moveItem(at: target, to: backup)
                movedOldTarget = true
            }
            try fileManager.moveItem(at: staging, to: target)
            let outcome: RegistrationOutcome
            do {
                outcome = try launch(target)
            } catch {
                do {
                    try quarantineFailedTarget()
                    try restoreBackupIfNeeded(movedOldTarget: movedOldTarget)
                } catch let rollbackError {
                    throw rollbackFailure(original: error, rollback: rollbackError)
                }
                throw error
            }
            if movedOldTarget, outcome == .registered {
                do { try fileManager.removeItem(at: backup) }
                catch {
                    throw CleanupFailure(path: backup.path, underlying: error)
                }
            }
            return outcome
        } catch {
            if error is CleanupFailure { throw error }
            do {
                try restoreBackupIfNeeded(movedOldTarget: movedOldTarget)
            } catch let rollbackError {
                throw rollbackFailure(original: error, rollback: rollbackError)
            }
            do { try fileManager.removeItem(at: staging) }
            catch {
                throw CleanupFailure(path: staging.path, underlying: error)
            }
            throw error
        }
    }

    private func quarantineFailedTarget() throws {
        guard fileManager.fileExists(atPath: target.path) else { return }
        let failedTarget = target.deletingLastPathComponent()
            .appending(path: ".\(target.lastPathComponent).failed-\(UUID().uuidString)")
        try fileManager.moveItem(at: target, to: failedTarget)
    }

    private func restoreBackupIfNeeded(movedOldTarget: Bool) throws {
        guard movedOldTarget, fileManager.fileExists(atPath: backup.path) else { return }
        if fileManager.fileExists(atPath: target.path) { try quarantineFailedTarget() }
        try fileManager.moveItem(at: backup, to: target)
    }

    private func rollbackFailure(original: Error, rollback: Error) -> Error {
        let backupState = fileManager.fileExists(atPath: backup.path)
            ? "backup preservado em \(backup.path)"
            : "backup não encontrado em \(backup.path)"
        let targetState = fileManager.fileExists(atPath: target.path)
            ? "alvo ainda existe em \(target.path)"
            : "alvo ausente em \(target.path)"
        let details = "Falha original: \(original.localizedDescription). "
            + "Rollback falhou: \(rollback.localizedDescription). "
            + "\(backupState); \(targetState). Intervenção manual necessária."
        return RegistrationFailure(message: details)
    }
}

enum StartupAction: Equatable {
    case installAndRegister
    case registerExisting
    case unregister
}

enum StartupDecision {
    static func action(isTemporaryApp: Bool, isCurrentlyEnabled: Bool) -> StartupAction {
        if isTemporaryApp { return .installAndRegister }
        return isCurrentlyEnabled ? .unregister : .registerExisting
    }
}

enum SnapshotPresentation {
    static func resetRows(_ credits: ResetCredits?, fallbackTitle: String) -> [(title: String, value: String)] {
        guard let credits, let available = credits.available, available > 0 else { return [] }
        if let groups = credits.expiryGroups {
            return groups.filter { $0.available > 0 }.map {
                (fallbackTitle, "\($0.available) · vence \($0.expiresAtRaw)")
            }
        }
        let expiry = credits.nextExpiry.map { " · vence \($0)" } ?? ""
        return [(fallbackTitle, "\(available)\(expiry)")]
    }
}
