import Foundation
import Testing
@testable import CostsPreview

@Suite("Decodificação do contrato status")
struct StatusTests {
    @Test func multiWindowProviderRetainsPaceResetAndActualPlan() throws {
        let snapshot = try decode(#"""
        {
          "month":"2026-10","spend_usd":13.2,"subscriptions_usd_month":34,
          "sync_age":"13h02","stale":true,"use_order":["claude"],
          "providers":[{"id":"claude","plan":"Max","windows":[
            {"label":"5h","used_pct":61,"color":"orange","reset":"2h",
             "resets_at":2000000000,"spare_pct":18,"text":"5h 61% usado",
             "hint":"no limite","wait":null},
            {"label":"sem","used_pct":23,"color":"green","reset":"4d",
             "resets_at":2001000000,"text":"semanal","hint":"no ritmo"}],
            "agents":[{"label":"mac / claude","usd":3.5,"tokens":1234}],
            "cost_usd":13.2,"subscription_usd_month":20,"balance":null,
            "ok":true,"stale":false,"priority_hint":"Ordem semanal"}]}
        """#)
        #expect(snapshot.providers.count == 1)
        #expect(snapshot.providers[0].plan == "Max")
        #expect(snapshot.providers[0].windows.map(\.label) == ["5h", "sem"])
        #expect(snapshot.providers[0].windows[0].usedPercent == 61)
        #expect(snapshot.providers[0].windows[0].hint == "no limite")
        #expect(snapshot.providers[0].subscriptionUSDMonth == 20)
        #expect(snapshot.providers[0].balance == nil)
    }

    @Test func balanceOnlyAndUnknownProviderArePreserved() throws {
        let snapshot = try decode(#"{"providers":[{"id":"new-provider","balance":"US$ 5.00","windows":[]}, {"id":"deepseek","balance":"US$ 1.00"}]}"#)
        #expect(snapshot.providers.count == 2)
        #expect(snapshot.providers[0].id == "new-provider")
        #expect(snapshot.providers[0].balance == "US$ 5.00")
        #expect(snapshot.providers[1].windows.isEmpty)
        #expect(snapshot.providers[1].costUSD == nil)
        #expect(ProviderCatalog.name(for: "new-provider") == "new-provider")
    }

    @Test func explicitKindsAndLegacyLabelsProduceOnlyDistinctRings() throws {
        let snapshot = try decode(#"""
        {"providers":[{"id":"codex","windows":[
          {"label":"short","kind":"session","used_pct":11},
          {"label":"long","kind":"week","used_pct":22},
          {"label":"bill","kind":"month","used_pct":33},
          {"label":"extra","kind":"other","used_pct":44}]}]}
        """#)
        let rings = GaugeRing.select(from: try #require(snapshot.providers.first).windows)
        #expect(rings.map(\.kind) == [.month, .week, .session, .other])
        #expect(rings.map(\.label) == ["bill", "long", "short", "extra"])

        let legacy = try decode(#"{"providers":[{"id":"claude","windows":[{"label":"5h","used_pct":10},{"label":"sem","used_pct":20}]}]}"#)
        let legacyRings = GaugeRing.select(from: try #require(legacy.providers.first).windows)
        #expect(legacyRings.map(\.kind) == [.week, .session])
    }

    @Test func unknownWindowsDoNotInventRingsAndExplicitKindWins() throws {
        let snapshot = try decode(#"""
        {"providers":[{"id":"xai","windows":[
          {"label":"custom","used_pct":25},
          {"label":"5h","kind":"other","used_pct":40},
          {"label":"5h","used_pct":80},
          {"label":"unknown","kind":"other","used_pct":70}]}]}
        """#)
        let rings = GaugeRing.select(from: try #require(snapshot.providers.first).windows)
        #expect(rings.map(\.kind) == [.session, .other])
        #expect(rings.map(\.usedPercent) == [80, 70])
    }

    @Test func rustShapedStatusFixturePreservesProviderWindowKindsAndRawExpiryGroups() throws {
        let snapshot = try decode(#"""
        {"providers":[
          {"id":"xai","windows":[{"label":"Grok","used_pct":14,"kind":"session"}]},
          {"id":"zai","windows":[
            {"label":"Sessão","used_pct":32,"kind":"session","resets_at":null},
            {"label":"Semana","used_pct":18,"kind":"week","resets_at":null}],
           "resets":{"available":3,"next_expiry":null,"next_expiry_at":null,
             "expiry_groups":[{"available":2,"expires_at_raw":"2026-11-02 09:00"},{"available":1,"expires_at_raw":"2026-11-09 09:00"}]}},
          {"id":"opencode-go","windows":[
            {"label":"5h","used_pct":11,"kind":"session"},
            {"label":"sem","used_pct":22,"kind":"week"},
            {"label":"mês","used_pct":33,"kind":"month"}]}
        ]}
        """#)
        let grok = try #require(snapshot.providers.first { $0.id == "xai" })
        #expect(GaugeRing.select(from: grok.windows).map(\.kind) == [.session])
        let zai = try #require(snapshot.providers.first { $0.id == "zai" })
        #expect(zai.windows.map(\.label) == ["Sessão", "Semana"])
        #expect(zai.windows.allSatisfy { $0.resetsAt == nil })
        let rawRows = SnapshotPresentation.resetRows(zai.resets, fallbackTitle: "Créditos")
        #expect(rawRows.map(\.value) == ["2 · vence 2026-11-02 09:00", "1 · vence 2026-11-09 09:00"])
        let opencode = try #require(snapshot.providers.first { $0.id == "opencode-go" })
        #expect(GaugeRing.select(from: opencode.windows).map(\.kind) == [.month, .week, .session])
    }

    @Test func quotaRefreshForcesOnlyOnExplicitAction() async throws {
        let runner = RecordingCommandRunner()
        let loader = StatusLoader(snapshotURL: nil, executableURL: URL(fileURLWithPath: "/bin/true"), runner: runner)
        try await loader.refreshQuota(force: false)
        try await loader.refreshQuota(force: true)
        #expect(runner.calls == [["quota", "--if-stale"], ["quota", "--refresh"]])
    }

    @Test func resetExpiryGroupsFallbackAndOnlyShowAvailableCredits() throws {
        let grouped = try decode(#"""
        {"providers":[{"id":"claude","resets":{"available":4,"next_expiry":"legacy",
          "expiry_groups":[{"available":2,"expires_at_raw":"2026-10-24 09:00"},
            {"available":0,"expires_at_raw":"2026-10-25 09:00"}]}}]}
        """#)
        let credits = try #require(grouped.providers.first?.resets)
        let groupedRows = SnapshotPresentation.resetRows(credits, fallbackTitle: "Resets")
        #expect(groupedRows.map(\.title) == ["Resets"])
        #expect(groupedRows.map(\.value) == ["2 · vence 2026-10-24 09:00"])
        let legacy = try decode(#"{"providers":[{"id":"codex","resets_week":{"available":3,"next_expiry":"amanhã"}}]}"#)
        let legacyRows = SnapshotPresentation.resetRows(legacy.providers.first?.resetsWeek, fallbackTitle: "Semana")
        #expect(legacyRows.map(\.title) == ["Semana"])
        #expect(legacyRows.map(\.value) == ["3 · vence amanhã"])
        #expect(SnapshotPresentation.resetRows(nil, fallbackTitle: "Resets").isEmpty)
    }

    @Test @MainActor func recommendationOrderingUsesUniqueOrderAndAlphabeticalRemainder() throws {
        let snapshot = try decode(#"""
        {"use_order":["codex","claude","codex"],"providers":[
          {"id":"xai"},{"id":"zai"},{"id":"claude"},{"id":"codex"}]}
        """#)
        let sorted = PreviewModel.recommendedProviders(snapshot.providers, useOrder: snapshot.useOrder ?? [])
        #expect(sorted.map(\.id) == ["codex", "claude", "xai", "zai"])
    }

    @Test func startupHelpersCanonicalizeTemporaryPathsAndChooseExplicitActions() {
        let paths = StartupPaths.resolve(homeDirectory: URL(fileURLWithPath: "/Users/test"), temporaryDirectory: URL(fileURLWithPath: "/private/tmp"))
        #expect(paths.isTemporaryApp(URL(fileURLWithPath: "/tmp/CostsPreview.app")))
        #expect(paths.isTemporaryApp(URL(fileURLWithPath: "/private/tmp/CostsPreview.app")))
        #expect(paths.isTemporaryApp(URL(fileURLWithPath: "/tmp/CostsPreview.app")))
        #expect(!paths.isTemporaryApp(URL(fileURLWithPath: "/Users/test/Applications/CostsPreview.app")))
        #expect(paths.installedApp.path == "/Users/test/Applications/CostsPreview.app")
        #expect(StartupDecision.action(isTemporaryApp: true, isCurrentlyEnabled: false) == .installAndRegister)
        #expect(StartupDecision.action(isTemporaryApp: false, isCurrentlyEnabled: false) == .registerExisting)
        #expect(StartupDecision.action(isTemporaryApp: false, isCurrentlyEnabled: true) == .unregister)
    }

    @Test func statusClassificationDoesNotGuessFromUnrelatedErrors() throws {
        let snapshot = try decode(#"""
        {"stale":true,"providers":[
          {"id":"a","ok":false,"error_text":"network timeout"},
          {"id":"b","ok":false,"error_text":"session expired"},
          {"id":"c","windows":[{"label":"custom","used_pct":null}]}]}
        """#)
        #expect(snapshot.stale == true)
        #expect(snapshot.providers[0].errorText == "network timeout")
        #expect(snapshot.providers[1].errorText == "session expired")
        #expect(snapshot.providers[2].windows[0].usedPercent == nil)
    }

    @Test func staleAndErrorRetainCachedWindows() throws {
        let snapshot = try decode(#"""
        {"providers":[{"id":"codex","ok":false,"error_text":"Sessão expirada",
          "error_hint":"Entre novamente","stale":true,
          "windows":[{"label":"sem","used_pct":42,"reset":"2d"}]}]}
        """#)
        let provider = try #require(snapshot.providers.first)
        #expect(provider.stale == true)
        #expect(provider.ok == false)
        #expect(provider.errorText == "Sessão expirada")
        #expect(provider.errorHint == "Entre novamente")
        #expect(provider.windows.first?.usedPercent == 42)
    }

    @Test func nullAndMissingFieldsStayUnavailableNotZero() throws {
        let snapshot = try decode(#"{"providers":[{"id":"openrouter","cost_usd":null,"subscription_usd_month":null,"balance":null,"windows":[{"label":"weekly","used_pct":null,"resets_at":null}]}]}"#)
        let provider = try #require(snapshot.providers.first)
        #expect(provider.costUSD == nil)
        #expect(provider.plan == nil)
        #expect(provider.balance == nil)
        #expect(provider.subscriptionUSDMonth == nil)
        #expect(provider.windows[0].usedPercent == nil)
        #expect(DisplayFormat.usd(provider.costUSD) == "Indisponível")
        #expect(DisplayFormat.percentage(provider.windows[0].usedPercent) == "Indisponível")
    }

    @Test func optionalPlanDoesNotBreakPreviousContractOrWhitespace() throws {
        let old = try decode(#"{"providers":[{"id":"zai"}]}"#)
        #expect(old.providers.first?.plan == nil)
        let empty = try decode(#"{"providers":[{"id":"zai","plan":"  "}]}"#)
        #expect(empty.providers.first?.plan?.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty == true)
    }

    @Test func formattersAndResetCountdownHandleEdges() {
        #expect(DisplayFormat.usd(nil) == "Indisponível")
        #expect(DisplayFormat.usd(.infinity) == "Indisponível")
        #expect(DisplayFormat.percentage(-4) == "-4%")
        #expect(DisplayFormat.tokens(nil) == "Indisponível")
        #expect(DisplayFormat.tokens(-1) == "Indisponível")
        #expect(DisplayFormat.resetCountdown(until: nil) == nil)
        #expect(DisplayFormat.resetCountdown(until: 999, now: Date(timeIntervalSince1970: 1000)) == "em breve")
        #expect(DisplayFormat.resetCountdown(until: 1000 + 3600, now: Date(timeIntervalSince1970: 1000)) == "em 1h 0m")
    }

    @Test func registrationRunsWhileExclusiveInstanceClaimIsHeldAndReleasesOnFailure() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let lockURL = root.appending(path: "instance.lock")
        defer { try? FileManager.default.removeItem(at: root) }

        #expect(throws: RegistrationFailure.self) {
            try ProcessLifetimeLock.withRegistrationClaim(lockURL: lockURL) {
                #expect(!ProcessLifetimeLock.registrationClaimAvailable(lockURL: lockURL))
                throw RegistrationFailure(message: "register failed")
            }
        }
        #expect(ProcessLifetimeLock.registrationClaimAvailable(lockURL: lockURL))
    }

    @Test func stagedInstallRegistrationFailureRollsBackOldBundleAndApprovalKeepsBackup() throws {
        let fm = FileManager.default
        let root = fm.temporaryDirectory.appending(path: UUID().uuidString, directoryHint: .isDirectory)
        let target = root.appending(path: "CostsPreview.app", directoryHint: .isDirectory)
        let staging = root.appending(path: ".staging.app", directoryHint: .isDirectory)
        let backup = root.appending(path: ".backup.app", directoryHint: .isDirectory)
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: target.appending(path: "version"))
        defer { try? fm.removeItem(at: root) }
        let transaction = StagedInstallTransaction(fileManager: fm, target: target, staging: staging, backup: backup)

        #expect(throws: (any Error).self) {
            try transaction.install({ stage in
                try fm.createDirectory(at: stage, withIntermediateDirectories: true)
                try Data("new".utf8).write(to: stage.appending(path: "version"))
            }, launch: { _ in throw RegistrationFailure(message: "registration failed") })
        }
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "old")
        #expect(!fm.fileExists(atPath: backup.path))

        let result = try transaction.install({ stage in
            try fm.createDirectory(at: stage, withIntermediateDirectories: true)
            try Data("new".utf8).write(to: stage.appending(path: "version"))
        }, launch: { _ in .requiresApproval })
        #expect(result == .requiresApproval)
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "new")
        #expect(try String(contentsOf: backup.appending(path: "version"), encoding: .utf8) == "old")
    }

    @Test func processLifetimeLockIsAtomicConcurrentAndReacquirableAfterRelease() async throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString, directoryHint: .isDirectory)
        let lockURL = root.appending(path: "private/instance.lock")
        defer { try? FileManager.default.removeItem(at: root) }

        let releaseGate = DispatchSemaphore(value: 0)
        let claims = await withTaskGroup(of: Bool.self, returning: [Bool].self) { group in
            for _ in 0..<12 {
                group.addTask {
                    nonisolatedLockAttempt(gate: releaseGate, url: lockURL)
                }
            }
            for _ in 0..<12 { releaseGate.signal() }
            var results: [Bool] = []
            for await result in group { results.append(result) }
            return results
        }
        let winners = claims.filter { $0 }
        winners.forEach { _ = $0 }

        #expect(claims.filter { $0 }.count == 1)
        let held = try #require(ProcessLifetimeLock(lockURL: lockURL))
        #expect(!ProcessLifetimeLock.registrationClaimAvailable(lockURL: lockURL))
        held.release()
        #expect(ProcessLifetimeLock.registrationClaimAvailable(lockURL: lockURL))
        let reacquired = ProcessLifetimeLock(lockURL: lockURL)
        #expect(reacquired != nil)
        reacquired?.release()
    }

    @Test func rollbackQuarantinesUndeletableFailedTargetAndRestoresBackup() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let target = root.appending(path: "CostsPreview.app")
        let staging = root.appending(path: ".staging.app")
        let backup = root.appending(path: ".backup.app")
        let fm = FileManager.default
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: target.appending(path: "version"))
        defer { try? fm.removeItem(at: root) }
        let operations = FaultInjectingInstallOperations(fileManager: fm, failures: [.removeTarget])
        let transaction = StagedInstallTransaction(fileManager: operations, target: target, staging: staging, backup: backup)

        #expect(throws: RegistrationFailure.self) {
            try transaction.install({ stage in
                try fm.createDirectory(at: stage, withIntermediateDirectories: true)
                try Data("new".utf8).write(to: stage.appending(path: "version"))
            }, launch: { _ in throw RegistrationFailure(message: "ack failed") })
        }
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "old")
        let quarantined = try fm.contentsOfDirectory(at: root, includingPropertiesForKeys: nil)
            .first { $0.lastPathComponent.contains(".failed-") }
        #expect(try String(contentsOf: try #require(quarantined).appending(path: "version"), encoding: .utf8) == "new")
        #expect(!fm.fileExists(atPath: backup.path))
    }

    @Test func rollbackFailurePreservesBackupAndReportsRecoveryPaths() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let target = root.appending(path: "CostsPreview.app")
        let staging = root.appending(path: ".staging.app")
        let backup = root.appending(path: ".backup.app")
        let fm = FileManager.default
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: target.appending(path: "version"))
        defer { try? fm.removeItem(at: root) }
        let operations = FaultInjectingInstallOperations(fileManager: fm, failures: [.quarantine, .restore])
        let transaction = StagedInstallTransaction(fileManager: operations, target: target, staging: staging, backup: backup)

        do {
            _ = try transaction.install({ stage in
                try fm.createDirectory(at: stage, withIntermediateDirectories: true)
                try Data("new".utf8).write(to: stage.appending(path: "version"))
            }, launch: { _ in throw RegistrationFailure(message: "ack failed") })
            Issue.record("Expected rollback failure")
        } catch let error as RegistrationFailure {
            #expect(error.message.contains("Rollback falhou"))
            #expect(error.message.contains(backup.path))
            #expect(error.message.contains(target.path))
        }
        #expect(fm.fileExists(atPath: backup.path))
        #expect(try String(contentsOf: backup.appending(path: "version"), encoding: .utf8) == "old")
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "new")
    }

    @Test func committedInstallReportsBackupCleanupFailureWithoutRollingBack() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        let target = root.appending(path: "CostsPreview.app")
        let staging = root.appending(path: ".staging.app")
        let backup = root.appending(path: ".backup.app")
        let fm = FileManager.default
        try fm.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: target.appending(path: "version"))
        defer { try? fm.removeItem(at: root) }
        let operations = FaultInjectingInstallOperations(fileManager: fm, failures: [.removeBackup])
        let transaction = StagedInstallTransaction(fileManager: operations, target: target, staging: staging, backup: backup)

        do {
            _ = try transaction.install({ stage in
                try fm.createDirectory(at: stage, withIntermediateDirectories: true)
                try Data("new".utf8).write(to: stage.appending(path: "version"))
            }, launch: { _ in .registered })
            Issue.record("Expected cleanup failure")
        } catch let error as CleanupFailure {
            #expect(error.path == backup.path)
        }
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "new")
        #expect(try String(contentsOf: backup.appending(path: "version"), encoding: .utf8) == "old")
    }

    @Test func stagedInstallRestoresExistingBundleOnPrepareOrLaunchFailure() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString, directoryHint: .isDirectory)
        let target = root.appending(path: "CostsPreview.app", directoryHint: .isDirectory)
        let staging = root.appending(path: ".staging.app", directoryHint: .isDirectory)
        let backup = root.appending(path: ".backup.app", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: target.appending(path: "version"))
        defer { try? FileManager.default.removeItem(at: root) }

        let transaction = StagedInstallTransaction(target: target, staging: staging, backup: backup)
        #expect(throws: (any Error).self) {
            try transaction.install({ stage in
                try FileManager.default.createDirectory(at: stage, withIntermediateDirectories: true)
                try Data("new".utf8).write(to: stage.appending(path: "version"))
            }, launch: { _ in throw LoaderError.timeout })
        }
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "old")
        #expect(!FileManager.default.fileExists(atPath: staging.path))
        #expect(!FileManager.default.fileExists(atPath: backup.path))
    }

    @Test func stagedInstallCopyFailureLeavesExistingBundleIntact() throws {
        let root = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString, directoryHint: .isDirectory)
        let target = root.appending(path: "CostsPreview.app", directoryHint: .isDirectory)
        let staging = root.appending(path: ".staging.app", directoryHint: .isDirectory)
        let backup = root.appending(path: ".backup.app", directoryHint: .isDirectory)
        try FileManager.default.createDirectory(at: target, withIntermediateDirectories: true)
        try Data("old".utf8).write(to: target.appending(path: "version"))
        defer { try? FileManager.default.removeItem(at: root) }

        let transaction = StagedInstallTransaction(target: target, staging: staging, backup: backup)
        #expect(throws: (any Error).self) {
            try transaction.install({ _ in throw LoaderError.timeout }, launch: { _ in .registered })
        }
        #expect(try String(contentsOf: target.appending(path: "version"), encoding: .utf8) == "old")
    }

    @Test func loaderUsesConfiguredDeadlineAndPropagatesCancellation() async throws {
        let runner = DelayedCommandRunner(delay: .seconds(2))
        let loader = StatusLoader(snapshotURL: nil, timeout: .milliseconds(80), runner: runner)
        let start = ContinuousClock.now
        await #expect(throws: (any Error).self) { try await loader.load() }
        #expect(start.duration(to: .now) < .seconds(1))

        let cancelLoader = StatusLoader(snapshotURL: nil, timeout: .seconds(5), runner: runner)
        let task = Task { try await cancelLoader.load() }
        try await Task.sleep(for: .milliseconds(80))
        task.cancel()
        await #expect(throws: CancellationError.self) { try await task.value }
    }

    @Test func snapshotFileModeOnlyReadsFixtureAndDecodesIt() async throws {
        let file = FileManager.default.temporaryDirectory.appending(path: UUID().uuidString)
        try Data(#"{"providers":[{"id":"deepseek","balance":"US$ 2.00"}]}"#.utf8).write(to: file)
        defer { try? FileManager.default.removeItem(at: file) }
        let loader = StatusLoader(snapshotURL: file, executableURL: URL(fileURLWithPath: "/path/that/must/not/run"))
        let snapshot = try await loader.load()
        #expect(snapshot.providers.first?.balance == "US$ 2.00")
    }

    private struct FaultInjectingInstallOperations: InstallFileOperations {
        enum Failure { case removeTarget, quarantine, restore, removeBackup }
        let fileManager: FileManager
        let failures: [Failure]

        func fileExists(atPath path: String) -> Bool { fileManager.fileExists(atPath: path) }

        func moveItem(at source: URL, to destination: URL) throws {
            if source.lastPathComponent == ".backup.app", failures.contains(where: { if case .restore = $0 { return true }; return false }) {
                throw TestInstallError.injected("restore")
            }
            if source.lastPathComponent == "CostsPreview.app", destination.lastPathComponent.contains(".failed-"),
               failures.contains(where: { if case .quarantine = $0 { return true }; return false }) {
                throw TestInstallError.injected("quarantine")
            }
            try fileManager.moveItem(at: source, to: destination)
        }

        func removeItem(at url: URL) throws {
            guard fileManager.fileExists(atPath: url.path) else { return }
            if url.lastPathComponent == "CostsPreview.app",
               failures.contains(where: { if case .removeTarget = $0 { return true }; return false }) {
                throw TestInstallError.injected("remove target")
            }
            if url.lastPathComponent == ".backup.app",
               failures.contains(where: { if case .removeBackup = $0 { return true }; return false }) {
                throw TestInstallError.injected("remove backup")
            }
            try fileManager.removeItem(at: url)
        }
    }

    private enum TestInstallError: Error, LocalizedError {
        case injected(String)
        var errorDescription: String? {
            if case let .injected(message) = self { return "injected \(message) failure" }
            return nil
        }
    }

    nonisolated private func nonisolatedLockAttempt(gate: DispatchSemaphore, url: URL) -> Bool {
        gate.wait()
        guard let lock = ProcessLifetimeLock(lockURL: url) else { return false }
        Thread.sleep(forTimeInterval: 0.1)
        lock.release()
        return true
    }

    private func decode(_ text: String) throws -> StatusSnapshot {
        try JSONDecoder().decode(StatusSnapshot.self, from: Data(text.utf8))
    }
}
