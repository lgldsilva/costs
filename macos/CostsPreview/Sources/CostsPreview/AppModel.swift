import Foundation
import SwiftUI

@MainActor
@Observable
final class PreviewModel {
    let loader: StatusLoader
    let isFixture: Bool
    var snapshot: StatusSnapshot?
    var selectedID: String? {
        didSet { if selectedID != oldValue { onSummaryChange?() } }
    }
    /// Avisar o AppDelegate para atualizar o título da barra (percentuais do selecionado).
    var onSummaryChange: (() -> Void)?
    var errorMessage: String?
    var isLoading = false
    private var loadTask: Task<Void, Never>?

    init(loader: StatusLoader, isFixture: Bool, initialProviderID: String? = nil) {
        self.loader = loader
        self.isFixture = isFixture
        self.selectedID = initialProviderID
    }

    var availableProviders: [Provider] {
        Self.recommendedProviders(snapshot?.providers ?? [], useOrder: snapshot?.useOrder ?? [])
    }

    nonisolated static func recommendedProviders(_ providers: [Provider], useOrder: [String]) -> [Provider] {
        let order = Dictionary(useOrder.enumerated().map { ($1, $0) }, uniquingKeysWith: min)
        return providers.sorted { left, right in
            let leftIndex = order[left.id] ?? useOrder.count
            let rightIndex = order[right.id] ?? useOrder.count
            return leftIndex == rightIndex ? left.id < right.id : leftIndex < rightIndex
        }
    }
    var selectedProvider: Provider? {
        availableProviders.first { $0.id == selectedID } ?? availableProviders.first
    }
    var recommendationRank: Int? {
        guard let selectedID, let order = snapshot?.useOrder,
              let index = order.firstIndex(of: selectedID) else { return nil }
        return index + 1
    }

    func loadIfNeeded() {
        guard snapshot == nil else { return }
        refresh()
    }

    /// Como `refresh` trata as cotas antes de reler o status.
    enum QuotaUpdate { case none, ifStale, force }

    /// Atualiza cotas na rede (só modo real) e relê o status do cache.
    func refresh(quota: QuotaUpdate = .none) {
        guard !isLoading else { return }
        isLoading = true
        errorMessage = nil
        loadTask = Task {
            defer { isLoading = false }
            do {
                if quota != .none { try await loader.refreshQuota(force: quota == .force) }
                let next = try await loader.load()
                snapshot = next
                if !availableProviders.contains(where: { $0.id == selectedID }) {
                    selectedID = availableProviders.first?.id
                }
                onSummaryChange?()
            } catch {
                errorMessage = error.localizedDescription
            }
        }
    }

    func cancel() { loadTask?.cancel() }
}
